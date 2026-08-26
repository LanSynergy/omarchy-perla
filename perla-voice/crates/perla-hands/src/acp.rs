//! Minimal ACP (Agent Client Protocol) client: JSON-RPC 2.0 over the child
//! process's stdin/stdout, one message per line. This is the entire wire
//! contract perla needs from a forked/upstream `grok` binary — by talking the
//! protocol instead of linking the fork's code, upstream syncs stay trivial.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// A notification pushed by the agent (e.g. `session/update`).
#[derive(Debug)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Clonable handle to one spawned agent process. Dropping every clone does
/// NOT kill the child — call [`AcpClient::kill`] for that.
#[derive(Clone)]
pub struct AcpClient {
    to_child: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    child: Arc<Mutex<Option<Child>>>,
}

impl AcpClient {
    /// Spawn `binary args…` in `cwd` and wire up the JSON-RPC plumbing.
    /// Returns the client plus the stream of server-initiated notifications.
    pub fn spawn(
        binary: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<(AcpClient, mpsc::UnboundedReceiver<Notification>)> {
        let mut child = Command::new(binary)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", binary.display()))?;

        let mut stdin = child.stdin.take().context("child stdin")?;
        let stdout = child.stdout.take().context("child stdout")?;
        let stderr = child.stderr.take().context("child stderr")?;

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
        let (notif_tx, notif_rx) = mpsc::unbounded_channel::<Notification>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Writer: serialize everything through one task so lines never
        // interleave.
        tokio::spawn(async move {
            while let Some(line) = write_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        // Stderr: the agent's logs — keep them out of the protocol but
        // visible at debug level.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "perla_hands::grok", "{line}");
            }
        });

        // Reader: route responses to their waiters, answer the few requests
        // the agent may send US, forward notifications.
        let reader_pending = pending.clone();
        let reader_write = write_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    debug!("non-JSON line from agent: {line}");
                    continue;
                };
                let method = msg.get("method").and_then(|m| m.as_str());
                let id = msg.get("id").cloned().filter(|v| !v.is_null());
                match (method, id) {
                    // A request FROM the agent (permission asks, fs, …).
                    (Some(method), Some(id)) => {
                        let reply = answer_agent_request(method, msg.get("params"));
                        let mut resp = json!({ "jsonrpc": "2.0", "id": id });
                        match reply {
                            Ok(result) => resp["result"] = result,
                            Err(message) => {
                                resp["error"] = json!({ "code": -32601, "message": message })
                            }
                        }
                        let _ = reader_write.send(resp.to_string());
                    }
                    // A notification.
                    (Some(method), None) => {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        if notif_tx
                            .send(Notification {
                                method: method.to_string(),
                                params,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    // A response to one of OUR requests.
                    (None, Some(id)) => {
                        let Some(id) = id.as_u64() else { continue };
                        let waiter = reader_pending.lock().unwrap().remove(&id);
                        if let Some(waiter) = waiter {
                            let outcome = if let Some(err) = msg.get("error") {
                                let text = err
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("agent error")
                                    .to_string();
                                Err(text)
                            } else {
                                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = waiter.send(outcome);
                        }
                    }
                    _ => {}
                }
            }
            // Stdout closed → the process is gone; fail every waiter so no
            // submit hangs forever. The dropped notif_tx tells the session.
            let waiters: Vec<_> = reader_pending.lock().unwrap().drain().collect();
            for (_, waiter) in waiters {
                let _ = waiter.send(Err("agent process exited".into()));
            }
        });

        Ok((
            AcpClient {
                to_child: write_tx,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                child: Arc::new(Mutex::new(Some(child))),
            },
            notif_rx,
        ))
    }

    /// Send a request and wait (up to `timeout`) for its response.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.to_child.send(line.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("agent process is gone"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(anyhow!("{method}: {message}")),
            Ok(Err(_)) => Err(anyhow!("{method}: agent process exited")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("{method}: timed out"))
            }
        }
    }

    /// Fire-and-forget notification (e.g. `session/cancel`).
    pub fn notify(&self, method: &str, params: Value) {
        let line = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.to_child.send(line.to_string());
    }

    /// Terminate the child process.
    pub fn kill(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.start_kill();
        }
    }
}

/// The agent runs with `--always-approve` so permission asks shouldn't
/// happen — but if one does, auto-allow instead of hanging the turn, and
/// reject anything else (we advertise no fs/terminal capabilities).
fn answer_agent_request(method: &str, params: Option<&Value>) -> Result<Value, String> {
    if method == "session/request_permission" {
        let options = params
            .and_then(|p| p.get("options"))
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default();
        let pick = options
            .iter()
            .find(|o| {
                o.get("kind")
                    .and_then(|k| k.as_str())
                    .map(|k| k.starts_with("allow"))
                    .unwrap_or(false)
            })
            .or_else(|| options.first())
            .and_then(|o| o.get("optionId"))
            .cloned();
        return match pick {
            Some(option_id) => Ok(json!({
                "outcome": { "outcome": "selected", "optionId": option_id }
            })),
            None => Ok(json!({ "outcome": { "outcome": "cancelled" } })),
        };
    }
    warn!("agent sent unsupported request '{method}'");
    Err(format!("client does not support '{method}'"))
}
