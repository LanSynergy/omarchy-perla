//! Live smoke test against the running herdr server:
//! 1. snapshot the board (workspaces + agents),
//! 2. run a command in a fresh visible tab and read its output,
//! 3. start a real grok agent in a tab, give it a task, watch the board
//!    watcher report working → idle, then clean everything up.
//!
//!     cargo run -p perla-herdr --example board_smoke

use std::time::Duration;

use perla_herdr::board::TrackedCommand;
use perla_herdr::{BoardWatcher, HerdrClient, HerdrEvent, TrackedCommands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = HerdrClient::new().expect("herdr binary");

    // 1. Board snapshot.
    let workspaces = client.workspaces().await?;
    let agents = client.agents().await?;
    println!("workspaces:");
    for w in &workspaces {
        println!("  {} '{}' focused={}", w.workspace_id, w.label, w.focused);
    }
    println!("agents on the board: {}", agents.len());
    for a in &agents {
        println!("  {} [{}] {} in {}", a.target(), a.agent, a.agent_status, a.cwd);
    }

    let perla_ws = workspaces
        .iter()
        .find(|w| w.label.eq_ignore_ascii_case("perla"))
        .map(|w| w.workspace_id.clone())
        .expect("a 'Perla' workspace");

    // 2. Visible command tab.
    let (cmd_tab, cmd_pane) = client
        .tab_create(&perla_ws, "/tmp", "smoke-cmd")
        .await?;
    client
        .wait_for_prompt(&cmd_pane, Duration::from_secs(10))
        .await?;
    client.pane_run(&cmd_pane, "echo board-smoke-ok").await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let out = client.read_target(&cmd_pane, 10).await?;
    println!(
        "run_command output seen: {}",
        out.contains("board-smoke-ok")
    );
    client.tab_close(&cmd_tab).await?;

    // 3. Watcher up (with the command registry), baseline established.
    let tracked: TrackedCommands = Default::default();
    let mut events = BoardWatcher::start(client.clone(), tracked.clone());
    tokio::time::sleep(Duration::from_millis(2500)).await; // let it baseline

    // 3a. Tracked command that FAILS — watcher must report exit code 3.
    let (fail_tab, fail_pane) = client
        .tab_create(&perla_ws, "/tmp", "smoke-fail")
        .await?;
    client
        .wait_for_prompt(&fail_pane, Duration::from_secs(10))
        .await?;
    client
        .pane_run(
            &fail_pane,
            "echo boom-diagnostic; exit-nope 2>/dev/null; sh -c 'exit 3'; printf '__perla_exit=%s\\n' $?",
        )
        .await?;
    tracked.lock().unwrap().push(TrackedCommand {
        pane_id: fail_pane.clone(),
        label: "smoke-fail".into(),
        command: "sh -c 'exit 3'".into(),
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let event = tokio::select! {
            e = events.recv() => e,
            _ = tokio::time::sleep_until(deadline) => {
                println!("TIMED OUT waiting for CommandFinished");
                break;
            }
        };
        if let Some(HerdrEvent::CommandFinished { label, exit_code, tail, .. }) = event {
            println!(
                "event: command '{label}' exited {exit_code}; tail has diagnostic: {}",
                tail.contains("boom-diagnostic")
            );
            break;
        }
    }
    client.tab_close(&fail_tab).await?;

    let (agent_tab, agent_pane) = client
        .tab_create(&perla_ws, "/tmp", "smoke-agent")
        .await?;
    client
        .wait_for_prompt(&agent_pane, Duration::from_secs(10))
        .await?;
    println!("starting grok in {agent_pane}…");
    client.agent_start("smoke-grok", "grok", &agent_pane).await?;
    println!("grok ready — sending task");
    client
        .agent_prompt("smoke-grok", "Reply with exactly the word READY and nothing else.")
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut saw_working = false;
    loop {
        let event = tokio::select! {
            e = events.recv() => e,
            _ = tokio::time::sleep_until(deadline) => {
                println!("TIMED OUT waiting for the idle transition");
                break;
            }
        };
        match event {
            Some(HerdrEvent::AgentAppeared { target, kind, workspace }) => {
                println!("event: appeared {kind} '{target}' in {workspace}");
            }
            Some(HerdrEvent::AgentStatus { target, from, to, .. }) => {
                println!("event: {target} {from} → {to}");
                if to == "working" {
                    saw_working = true;
                }
                if saw_working && (to == "idle" || to == "done") {
                    println!("turn observed end-to-end ✓");
                    break;
                }
            }
            Some(HerdrEvent::AgentGone { target, .. }) => {
                println!("event: gone '{target}'");
            }
            Some(_) => {}
            None => break,
        }
    }

    let reply = client.read_target("smoke-grok", 30).await.unwrap_or_default();
    println!("pane tail contains READY: {}", reply.contains("READY"));

    client.tab_close(&agent_tab).await?;
    println!("cleaned up");
    Ok(())
}
