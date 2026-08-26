//! Realtime voice providers.
//!
//! The engine speaks the OpenAI Realtime JSON event dialect. With local API
//! keys there is no SDP relay: we connect straight over WebSocket
//! (`wss://.../v1/realtime?model=...`) — the direct replacement for the
//! macOS app's WebRTC + Supabase-edge-function path. Audio rides the same
//! socket as base64 PCM16 (`input_audio_buffer.append` /
//! `response.output_audio.delta`).
//!
//! Grok (xAI) exposes an OpenAI-compatible realtime dialect, so it is the
//! same transport with a different endpoint/key; provider-specific quirks
//! belong in this crate and nowhere else.

pub mod events;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

/// Which dialect quirks to apply. Today both use the OpenAI shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Grok,
}

#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub dialect: Dialect,
    /// Base WebSocket URL, e.g. "wss://api.openai.com/v1/realtime".
    pub url: String,
    pub api_key: String,
    /// Realtime model id, appended as `?model=`.
    pub model: String,
}

/// A live realtime connection. JSON in, JSON out — semantics live in the
/// engine. Dropping it (or calling `close`) tears down the socket.
pub struct Connection {
    outbound: mpsc::UnboundedSender<Value>,
    /// Inbound server events, in arrival order. Taken once by the engine's
    /// forwarder task via `take_events`.
    events: Option<mpsc::UnboundedReceiver<Value>>,
    close: tokio::sync::watch::Sender<bool>,
}

impl Connection {
    /// Queue a client event for sending. Non-blocking; returns false when the
    /// socket is already gone (callers treat that as a transport drop).
    pub fn send(&self, event: Value) -> bool {
        self.outbound.send(event).is_ok()
    }

    /// A clonable sender for high-frequency producers (the audio pipe) so
    /// they can bypass the engine's event loop.
    pub fn outbound_sender(&self) -> mpsc::UnboundedSender<Value> {
        self.outbound.clone()
    }

    /// The inbound event stream. When it yields `None` the transport is dead.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<Value>> {
        self.events.take()
    }

    pub fn close(&self) {
        let _ = self.close.send(true);
    }
}

pub async fn connect(settings: &ProviderSettings) -> Result<Connection> {
    if settings.api_key.is_empty() {
        return Err(anyhow!(
            "no API key configured for {:?} (set PERLA_OPENAI_API_KEY / PERLA_XAI_API_KEY or the config file)",
            settings.dialect
        ));
    }
    let url = format!("{}?model={}", settings.url, settings.model);
    let mut request = url
        .clone()
        .into_client_request()
        .context("building websocket request")?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", settings.api_key)
            .parse()
            .context("api key contains invalid header characters")?,
    );

    let (ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    debug!(status = %response.status(), "realtime websocket connected");

    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
    let (close_tx, mut close_rx) = tokio::sync::watch::channel(false);

    // Writer: serialize outbound events; exit on close or channel drop.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe = out_rx.recv() => {
                    let Some(event) = maybe else { break };
                    let Ok(text) = serde_json::to_string(&event) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                _ = close_rx.changed() => {
                    if *close_rx.borrow() {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    });

    // Reader: parse inbound frames into JSON events. Dropping `in_tx` (socket
    // closed / error) is how the engine learns the transport died.
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        if in_tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("unparseable realtime event: {e}"),
                },
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {} // ping/pong/binary — tungstenite answers pings itself
            }
        }
        debug!("realtime websocket reader ended");
    });

    Ok(Connection {
        outbound: out_tx,
        events: Some(in_rx),
        close: close_tx,
    })
}
