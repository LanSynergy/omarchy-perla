//! Live smoke test: spawn the real grok binary via the ACP bridge, run one
//! small task in a temp dir, print every event, verify the file it made.
//!
//!     cargo run -p perla-hands --example smoke

use perla_agents::orchestrator::AgentRunContext;
use perla_hands::{HandsEvent, HandsPool};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,perla_hands=debug".into()),
        )
        .init();

    let dir = std::env::temp_dir().join(format!("perla-hands-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    println!("workspace: {}", dir.display());

    let (pool, mut events) = HandsPool::new(None, None);
    let cancel_mode = std::env::args().nth(1).as_deref() == Some("cancel");

    let task = if cancel_mode {
        "Count from 1 to 50, writing each number to a separate file named n1.txt, n2.txt, and \
         so on, one file at a time."
    } else {
        "Create a file named hello.txt in the current directory containing exactly the \
         text 'hello from perla hands' and nothing else. Then reply with one short \
         sentence confirming it."
    };
    let outcome = pool
        .submit(
            dir.to_str().unwrap(),
            task,
            AgentRunContext::new(None),
            false,
        )
        .await;
    println!("submit → {outcome:?}");

    if cancel_mode {
        let pool2 = pool.clone();
        let cwd = dir.to_str().unwrap().to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            println!(">>> sending cancel");
            println!(">>> cancel accepted = {}", pool2.cancel(&cwd));
        });
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        let event = tokio::select! {
            e = events.recv() => e,
            _ = tokio::time::sleep_until(deadline) => {
                println!("TIMED OUT waiting for completion");
                break;
            }
        };
        let Some(event) = event else { break };
        match &event {
            HandsEvent::Progress {
                digest,
                elapsed_secs,
                ..
            } => {
                println!(
                    "  [{elapsed_secs:5.1}s] todos={} actions={:?} files={:?} msg={:?}",
                    digest.todos.len(),
                    digest.recent_actions.last(),
                    digest.changed_files,
                    digest
                        .last_message
                        .as_deref()
                        .map(|m| &m[..m.len().min(60)]),
                );
            }
            HandsEvent::Running { running, .. } => println!("running = {running}"),
            HandsEvent::QueuedStarted { prompt, .. } => println!("queued started: {prompt}"),
            HandsEvent::TurnFinished {
                outcome,
                changed_files,
                ..
            } => {
                println!(
                    "TURN FINISHED ok={} interrupted={}",
                    outcome.ok, outcome.interrupted
                );
                println!("  summary: {}", outcome.summary);
                println!("  changed: {changed_files:?}");
                let file = dir.join("hello.txt");
                match std::fs::read_to_string(&file) {
                    Ok(content) => println!("  hello.txt = {:?}", content.trim()),
                    Err(e) => println!("  hello.txt MISSING: {e}"),
                }
                break;
            }
        }
    }

    pool.terminate_all();
    Ok(())
}
