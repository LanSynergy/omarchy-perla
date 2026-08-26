//! Hidden PTY host for background agent sessions — port of
//! `HiddenAgentSession.swift` on `portable-pty` (cross-platform).
//!
//! The agent runs attached to a REAL pty so its TUI behaves exactly as in a
//! terminal, but nothing renders: Perla reads the JSONL transcript, not the
//! screen. Output is drained and discarded continuously — without that the
//! pty buffer fills (~16KB) and the agent blocks mid-write, which looks
//! exactly like a hung turn.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use tracing::debug;

use crate::paths;
use crate::types::AgentTool;

pub struct HiddenAgentSession {
    pub tool: AgentTool,
    pub cwd: String,
    pub pid: Option<u32>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    alive: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    /// Fires once when the process exits on its own (crash, /exit, external
    /// kill) — NOT via `terminate()`.
    pub exited: tokio::sync::watch::Receiver<bool>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl HiddenAgentSession {
    pub fn spawn(tool: AgentTool, cwd: &str, executable: &str, args: &[String]) -> Result<Self> {
        let pty = native_pty_system();
        // Generous fixed size — some TUIs cramp below ~80 cols, and nobody is
        // looking at the layout anyway.
        let pair = pty
            .openpty(PtySize {
                rows: 40,
                cols: 160,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(executable);
        cmd.args(args);
        cmd.cwd(cwd);
        for (k, v) in paths::terminal_environment() {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn agent in pty")?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();

        // Continuous drain: read and discard everything the TUI prints.
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        std::thread::Builder::new()
            .name("perla-pty-drain".into())
            .spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            })
            .ok();

        let writer = Arc::new(Mutex::new(pair.master.take_writer().context("pty writer")?));
        let alive = Arc::new(AtomicBool::new(true));
        let terminated = Arc::new(AtomicBool::new(false));
        let (exit_tx, exited) = tokio::sync::watch::channel(false);

        // Reaper thread: wait() the child so it never zombies, then signal.
        {
            let alive = alive.clone();
            std::thread::Builder::new()
                .name("perla-pty-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                    alive.store(false, Ordering::Relaxed);
                    let _ = exit_tx.send(true);
                })
                .ok();
        }

        debug!(tool = tool.id(), cwd, pid, "hidden agent spawned");
        Ok(Self {
            tool,
            cwd: cwd.to_string(),
            pid,
            writer,
            killer,
            alive,
            terminated,
            exited,
            _master: pair.master,
        })
    }

    /// Type a prompt into the live TUI. `\r` (not `\n`) in a SEPARATE write a
    /// beat later — one combined write races bracketed-paste and the prompt
    /// sits in the composer unsent.
    pub fn send_prompt(&self, text: &str) {
        self.write(text);
        let writer = self.writer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(180)).await;
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(b"\r");
                let _ = w.flush();
            }
        });
    }

    /// Esc — stops the agent generating but keeps the session at its prompt.
    pub fn send_interrupt(&self) {
        self.write("\u{1b}");
    }

    fn write(&self, s: &str) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(s.as_bytes());
            let _ = w.flush();
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.terminated.load(Ordering::Relaxed) && self.alive.load(Ordering::Relaxed)
    }

    /// True when `terminate()` initiated the shutdown (so the exit watch can
    /// tell a self-exit from an intentional one).
    pub fn was_terminated(&self) -> bool {
        self.terminated.load(Ordering::Relaxed)
    }

    /// Ctrl-C into the pty (clean CLI shutdown), then a hard kill as a
    /// backstop after 4s if the process is still up.
    pub fn terminate(&mut self) {
        if self.terminated.swap(true, Ordering::Relaxed) {
            return;
        }
        self.write("\u{03}");
        let mut killer = self.killer.clone_killer();
        let alive = self.alive.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            if alive.load(Ordering::Relaxed) {
                let _ = killer.kill();
            }
        });
    }
}
