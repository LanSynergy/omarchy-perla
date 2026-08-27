//! Gemini Multimodal Live API (BidiGenerateContent) dialect adaptor.
//!
//! Translates between Google Gemini 2.0 Bidi WebSocket frames and Perla's
//! internal OpenAI-style Realtime event shapes.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::{Connection, ProviderSettings};

/// Resolve voice to one of the 5 supported Gemini Live prebuilt voices:
/// Puck, Charon, Kore, Fenrir, Aoede. Defaults to Puck.
pub fn resolve_gemini_voice(voice: Option<&str>) -> &'static str {
    match voice.unwrap_or("Puck").trim().to_ascii_lowercase().as_str() {
        "puck" => "Puck",
        "charon" => "Charon",
        "kore" => "Kore",
        "fenrir" => "Fenrir",
        "aoede" => "Aoede",
        _ => "Puck",
    }
}

/// Format model identifier with the required `models/` prefix.
pub fn resolve_gemini_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.starts_with("models/") {
        trimmed.to_string()
    } else {
        format!("models/{trimmed}")
    }
}

/// Translate an engine event into a Gemini BidiGenerateContent client message.
pub fn translate_outbound(event: Value, default_model: &str) -> Option<Value> {
    let event_type = event.get("type").and_then(Value::as_str)?;

    match event_type {
        "session.update" => {
            let session = event.get("session")?;
            let instructions = session
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or("");
            let voice = session
                .pointer("/audio/output/voice")
                .and_then(Value::as_str);
            let voice_name = resolve_gemini_voice(voice);
            let model_name = resolve_gemini_model(default_model);

            let tools_raw = session.get("tools").and_then(Value::as_array);
            let function_declarations: Vec<Value> = tools_raw
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| {
                            let name = t
                                .get("name")
                                .or_else(|| t.pointer("/function/name"))
                                .and_then(Value::as_str)?;
                            let desc = t
                                .get("description")
                                .or_else(|| t.pointer("/function/description"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let params = t
                                .get("parameters")
                                .or_else(|| t.pointer("/function/parameters"))
                                .cloned()
                                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                            Some(json!({
                                "name": name,
                                "description": desc,
                                "parameters": params,
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();

            let mut setup = json!({
                "model": model_name,
                "generationConfig": {
                    "responseModalities": ["AUDIO"],
                    "speechConfig": {
                        "voiceConfig": {
                            "prebuiltVoiceConfig": {
                                "voiceName": voice_name
                            }
                        }
                    }
                },
                "systemInstruction": {
                    "parts": [{ "text": instructions }]
                }
            });

            if !function_declarations.is_empty() {
                setup["tools"] = json!([{
                    "functionDeclarations": function_declarations
                }]);
            }

            Some(json!({ "setup": setup }))
        }

        "input_audio_buffer.append" => {
            let audio_b64 = event.get("audio").and_then(Value::as_str)?;
            Some(json!({
                "realtimeInput": {
                    "mediaChunks": [
                        {
                            "mimeType": "audio/pcm;rate=24000",
                            "data": audio_b64
                        }
                    ]
                }
            }))
        }

        "conversation.item.create" => {
            let item = event.get("item")?;
            let item_type = item.get("type").and_then(Value::as_str)?;

            match item_type {
                "function_call_output" => {
                    let call_id = item.get("call_id").and_then(Value::as_str)?;
                    let output_str = item.get("output").and_then(Value::as_str).unwrap_or("{}");
                    let output_val: Value = serde_json::from_str(output_str)
                        .unwrap_or_else(|_| json!({ "output": output_str }));
                    let response_obj = if output_val.is_object() {
                        output_val
                    } else {
                        json!({ "output": output_val })
                    };

                    Some(json!({
                        "toolResponse": {
                            "functionResponses": [
                                {
                                    "id": call_id,
                                    "response": {
                                        "output": response_obj
                                    }
                                }
                            ]
                        }
                    }))
                }

                "message" => {
                    let mut parts = Vec::new();
                    if let Some(contents) = item.get("content").and_then(Value::as_array) {
                        for c in contents {
                            if let Some(text) = c.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    parts.push(json!({ "text": text }));
                                }
                            }
                            if let Some(img_url) = c.get("image_url").and_then(Value::as_str) {
                                let (mime, b64) = if let Some(stripped) = img_url.strip_prefix("data:") {
                                    if let Some((mime_part, rest)) = stripped.split_once(";base64,") {
                                        (mime_part, rest)
                                    } else {
                                        ("image/jpeg", stripped)
                                    }
                                } else {
                                    ("image/jpeg", img_url)
                                };
                                parts.push(json!({
                                    "inlineData": {
                                        "mimeType": mime,
                                        "data": b64
                                    }
                                }));
                            }
                        }
                    }

                    if parts.is_empty() {
                        None
                    } else {
                        Some(json!({
                            "clientContent": {
                                "turns": [
                                    {
                                        "role": "user",
                                        "parts": parts
                                    }
                                ],
                                "turnComplete": true
                            }
                        }))
                    }
                }

                _ => None,
            }
        }

        _ => None,
    }
}

/// Translate a Gemini BidiGenerateContent server message into Perla engine events.
pub fn translate_inbound(msg: Value) -> Vec<Value> {
    let mut events = Vec::new();

    // 1. Setup confirmation
    if msg.get("setupComplete").is_some() {
        events.push(json!({
            "type": "session.created",
            "session": {}
        }));
        return events;
    }

    // 2. Server Content (audio, transcript, turn completion, interruption)
    if let Some(server_content) = msg.get("serverContent") {
        if server_content.get("interrupted").and_then(Value::as_bool) == Some(true) {
            events.push(json!({
                "type": "input_audio_buffer.speech_started"
            }));
        }

        if let Some(model_turn) = server_content.get("modelTurn") {
            if let Some(parts) = model_turn.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if let Some(inline_data) = part.get("inlineData") {
                        if let Some(data) = inline_data.get("data").and_then(Value::as_str) {
                            let mime = inline_data
                                .get("mimeType")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if mime.is_empty() || mime.starts_with("audio/") {
                                events.push(json!({
                                    "type": "response.output_audio.delta",
                                    "delta": data
                                }));
                            }
                        }
                    }

                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            events.push(json!({
                                "type": "response.audio_transcript.done",
                                "transcript": text
                            }));
                        }
                    }
                }
            }
        }

        if server_content.get("turnComplete").and_then(Value::as_bool) == Some(true) {
            events.push(json!({
                "type": "response.done",
                "response": {
                    "output": [],
                    "usage": {
                        "input_token_details": { "text_tokens": 0, "audio_tokens": 0 },
                        "output_token_details": { "text_tokens": 0, "audio_tokens": 0 }
                    }
                }
            }));
        }
    }

    // 3. Tool Calls
    if let Some(tool_call) = msg.get("toolCall") {
        if let Some(function_calls) = tool_call.get("functionCalls").and_then(Value::as_array) {
            let output: Vec<Value> = function_calls
                .iter()
                .filter_map(|fc| {
                    let id = fc.get("id").and_then(Value::as_str)?;
                    let name = fc.get("name").and_then(Value::as_str)?;
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
                    Some(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args_str,
                    }))
                })
                .collect();

            if !output.is_empty() {
                events.push(json!({
                    "type": "response.done",
                    "response": {
                        "output": output,
                        "usage": {
                            "input_token_details": { "text_tokens": 0, "audio_tokens": 0 },
                            "output_token_details": { "text_tokens": 0, "audio_tokens": 0 }
                        }
                    }
                }));
            }
        }
    }

    // 4. Tool Call Cancellation
    if msg.get("toolCallCancellation").is_some() {
        events.push(json!({
            "type": "response.cancelled"
        }));
    }

    // 5. Error
    if let Some(error) = msg.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("gemini error");
        let code = error.get("code").and_then(Value::as_i64).map(|c| c.to_string());
        events.push(json!({
            "type": "error",
            "error": {
                "message": message,
                "code": code
            }
        }));
    }

    events
}

/// Connect to Google Gemini Multimodal Live API WebSocket.
pub async fn connect_gemini(settings: &ProviderSettings) -> Result<Connection> {
    if settings.api_key.is_empty() {
        return Err(anyhow!(
            "no API key configured for Gemini (set PERLA_GEMINI_API_KEY / GEMINI_API_KEY or the config file)"
        ));
    }

    let url = if settings.url.contains('?') {
        format!("{}&key={}", settings.url, settings.api_key)
    } else {
        format!("{}?key={}", settings.url, settings.api_key)
    };

    let mut request = url
        .clone()
        .into_client_request()
        .context("building websocket request")?;
    request.headers_mut().insert(
        "x-goog-api-key",
        settings
            .api_key
            .parse()
            .context("api key contains invalid header characters")?,
    );

    let (ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    debug!(status = %response.status(), "gemini realtime websocket connected");

    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
    let (close_tx, mut close_rx) = tokio::sync::watch::channel(false);

    let model = settings.model.clone();

    // Outbound task: translate engine events -> Gemini Bidi frames
    tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe = out_rx.recv() => {
                    let Some(event) = maybe else { break };
                    if let Some(gemini_msg) = translate_outbound(event, &model) {
                        let Ok(text) = serde_json::to_string(&gemini_msg) else { continue };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
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

    // Inbound task: translate Gemini Bidi frames -> engine events
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        let events = translate_inbound(v);
                        for event in events {
                            if in_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => warn!("unparseable gemini realtime event: {e}"),
                },
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        debug!("gemini realtime websocket reader ended");
    });

    Ok(Connection {
        outbound: out_tx,
        events: Some(in_rx),
        close: close_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_voice() {
        assert_eq!(resolve_gemini_voice(Some("Puck")), "Puck");
        assert_eq!(resolve_gemini_voice(Some("puck")), "Puck");
        assert_eq!(resolve_gemini_voice(Some("charon")), "Charon");
        assert_eq!(resolve_gemini_voice(Some("Kore")), "Kore");
        assert_eq!(resolve_gemini_voice(Some("fenrir")), "Fenrir");
        assert_eq!(resolve_gemini_voice(Some("Aoede")), "Aoede");
        // Unknown voice falls back to Puck
        assert_eq!(resolve_gemini_voice(Some("marin")), "Puck");
        assert_eq!(resolve_gemini_voice(None), "Puck");
    }

    #[test]
    fn test_resolve_model() {
        assert_eq!(
            resolve_gemini_model("models/gemini-2.0-flash-exp"),
            "models/gemini-2.0-flash-exp"
        );
        assert_eq!(
            resolve_gemini_model("gemini-2.0-flash"),
            "models/gemini-2.0-flash"
        );
    }

    #[test]
    fn test_translate_session_update_to_setup() {
        let update = json!({
            "type": "session.update",
            "session": {
                "instructions": "You are Perla.",
                "tools": [
                    {
                        "type": "function",
                        "name": "omarchy_run",
                        "description": "Run command",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "command": { "type": "string" }
                            }
                        }
                    }
                ],
                "audio": {
                    "output": {
                        "voice": "Charon"
                    }
                }
            }
        });

        let out = translate_outbound(update, "models/gemini-2.0-flash-exp").unwrap();
        assert_eq!(out.pointer("/setup/model").unwrap(), "models/gemini-2.0-flash-exp");
        assert_eq!(
            out.pointer("/setup/systemInstruction/parts/0/text").unwrap(),
            "You are Perla."
        );
        assert_eq!(
            out.pointer("/setup/generationConfig/speechConfig/voiceConfig/prebuiltVoiceConfig/voiceName").unwrap(),
            "Charon"
        );
        assert_eq!(
            out.pointer("/setup/tools/0/functionDeclarations/0/name").unwrap(),
            "omarchy_run"
        );
    }

    #[test]
    fn test_translate_audio_append() {
        let append = json!({
            "type": "input_audio_buffer.append",
            "audio": "AQIDBA=="
        });
        let out = translate_outbound(append, "models/gemini-2.0-flash").unwrap();
        assert_eq!(
            out.pointer("/realtimeInput/mediaChunks/0/mimeType").unwrap(),
            "audio/pcm;rate=24000"
        );
        assert_eq!(
            out.pointer("/realtimeInput/mediaChunks/0/data").unwrap(),
            "AQIDBA=="
        );
    }

    #[test]
    fn test_translate_user_message_and_image() {
        let msg = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "Screenshot caption" },
                    { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==" }
                ]
            }
        });
        let out = translate_outbound(msg, "models/gemini-2.0-flash").unwrap();
        assert_eq!(
            out.pointer("/clientContent/turns/0/parts/0/text").unwrap(),
            "Screenshot caption"
        );
        assert_eq!(
            out.pointer("/clientContent/turns/0/parts/1/inlineData/mimeType").unwrap(),
            "image/png"
        );
        assert_eq!(
            out.pointer("/clientContent/turns/0/parts/1/inlineData/data").unwrap(),
            "iVBORw0KGgoAAAANSUhEUg=="
        );
    }

    #[test]
    fn test_translate_function_call_output() {
        let tool_res = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "function_call_output",
                "call_id": "call_456",
                "output": "{\"ok\":true,\"workspace\":\"/home\"}"
            }
        });
        let out = translate_outbound(tool_res, "models/gemini-2.0-flash").unwrap();
        assert_eq!(
            out.pointer("/toolResponse/functionResponses/0/id").unwrap(),
            "call_456"
        );
        assert_eq!(
            out.pointer("/toolResponse/functionResponses/0/response/output/ok").unwrap(),
            true
        );
        assert_eq!(
            out.pointer("/toolResponse/functionResponses/0/response/output/workspace").unwrap(),
            "/home"
        );
    }

    #[test]
    fn test_translate_inbound_setup_complete() {
        let msg = json!({ "setupComplete": {} });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "session.created");
    }

    #[test]
    fn test_translate_inbound_audio_and_transcript() {
        let msg = json!({
            "serverContent": {
                "modelTurn": {
                    "parts": [
                        {
                            "inlineData": {
                                "mimeType": "audio/pcm;rate=24000",
                                "data": "pcm_data_here"
                            }
                        },
                        {
                            "text": "Hello world"
                        }
                    ]
                }
            }
        });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "response.output_audio.delta");
        assert_eq!(events[0]["delta"], "pcm_data_here");
        assert_eq!(events[1]["type"], "response.audio_transcript.done");
        assert_eq!(events[1]["transcript"], "Hello world");
    }

    #[test]
    fn test_translate_inbound_interruption() {
        let msg = json!({
            "serverContent": {
                "interrupted": true
            }
        });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "input_audio_buffer.speech_started");
    }

    #[test]
    fn test_translate_inbound_turn_complete() {
        let msg = json!({
            "serverContent": {
                "turnComplete": true
            }
        });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.done");
    }

    #[test]
    fn test_translate_inbound_tool_call() {
        let msg = json!({
            "toolCall": {
                "functionCalls": [
                    {
                        "id": "call_abc",
                        "name": "omarchy_run",
                        "args": { "command": "theme" }
                    }
                ]
            }
        });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.done");
        assert_eq!(events[0]["response"]["output"][0]["type"], "function_call");
        assert_eq!(events[0]["response"]["output"][0]["call_id"], "call_abc");
        assert_eq!(events[0]["response"]["output"][0]["name"], "omarchy_run");
        let args: Value = serde_json::from_str(events[0]["response"]["output"][0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "theme");
    }

    #[test]
    fn test_translate_inbound_tool_call_cancellation() {
        let msg = json!({
            "toolCallCancellation": {
                "ids": ["call_abc"]
            }
        });
        let events = translate_inbound(msg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.cancelled");
    }
}
