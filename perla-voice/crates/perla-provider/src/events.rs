//! Client → server Realtime event builders — port of `RealtimeEvents.swift`.
//! Only what the engine writes; inbound events stay dynamic JSON.

use serde_json::{json, Value};

/// Turn-taking tuning. `server_vad`, deliberately not `semantic_vad`:
/// semantic VAD ends the turn when a sentence *sounds* complete, which cuts
/// users off mid-thought. server_vad trades a little snappiness for one
/// deterministic knob: raise `silence_duration_ms` and it waits longer.
#[derive(Debug, Clone)]
pub struct VadParams {
    pub silence_duration_ms: u32,
    pub prefix_padding_ms: u32,
    pub threshold: f64,
}

#[derive(Debug, Clone)]
pub struct SessionLimits {
    pub max_output_tokens: u32,
    pub context_token_limit: u32,
    pub retention_ratio: f64,
}

impl Default for VadParams {
    fn default() -> Self {
        Self {
            silence_duration_ms: 1000,
            prefix_padding_ms: 300,
            threshold: 0.5,
        }
    }
}

impl VadParams {
    pub fn payload(&self) -> Value {
        json!({
            "type": "server_vad",
            "threshold": self.threshold,
            "prefix_padding_ms": self.prefix_padding_ms,
            "silence_duration_ms": self.silence_duration_ms,
            "create_response": true,
            // Server-side barge-in: truncate the in-flight response the moment
            // the user speaks. The engine ALSO clears the local playback queue
            // on speech_started — this flag alone leaves already-delivered
            // audio playing, which is what the user actually hears.
            "interrupt_response": true,
        })
    }
}

/// `session.update`. `voice` non-nil includes the `audio` block (turn
/// detection + voice + PCM16 formats). The `session.audio` block is REPLACED
/// wholesale, not merged — and `voice` is only accepted before the model's
/// first audio response — so only the FIRST update of a transport passes it;
/// mid-call re-pins (language lock) pass None and leave audio config alone.
pub fn session_update(
    instructions: &str,
    tools: &[Value],
    voice: Option<&str>,
    vad: &VadParams,
    limits: Option<&SessionLimits>,
) -> Value {
    let mut session = json!({
        "type": "realtime",
        "instructions": instructions,
        "tools": tools,
        "tool_choice": "auto",
    });
    if let Some(limits) = limits {
        session["max_output_tokens"] = json!(limits.max_output_tokens.clamp(1, 4096));
        session["truncation"] = json!({
            "type": "retention_ratio",
            "retention_ratio": limits.retention_ratio.clamp(0.5, 1.0),
            "token_limits": {
                "post_instructions": limits.context_token_limit.max(1_000),
            },
        });
    }
    if let Some(voice) = voice {
        session["audio"] = json!({
            "input": {
                "format": { "type": "audio/pcm", "rate": 24000 },
                "turn_detection": vad.payload(),
            },
            "output": {
                "format": { "type": "audio/pcm", "rate": 24000 },
                "voice": voice,
            },
        });
    }
    json!({ "type": "session.update", "session": session })
}

pub fn append_audio(base64_pcm16: &str) -> Value {
    json!({ "type": "input_audio_buffer.append", "audio": base64_pcm16 })
}

pub fn clear_input_audio() -> Value {
    json!({ "type": "input_audio_buffer.clear" })
}

pub fn create_user_message(text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        },
    })
}

pub fn create_system_message(text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "system",
            "content": [{ "type": "input_text", "text": text }],
        },
    })
}

/// A screenshot handed to the model as user content.
///
/// Realtime accepts an image only on a message item — never inside a
/// `function_call_output` — so a `see` call arrives in two parts: the function
/// output carrying the text facts, and this carrying the pixels.
pub fn create_user_image(item_id: &str, data_url: &str, caption: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "id": item_id,
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": caption },
                { "type": "input_image", "image_url": data_url },
            ],
        },
    })
}

pub fn delete_item(item_id: &str) -> Value {
    json!({
        "type": "conversation.item.delete",
        "item_id": item_id,
    })
}

pub fn create_function_output(call_id: &str, output: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        },
    })
}

/// `response.instructions` REPLACES the session instructions for this one
/// response — callers must staple the language clause on themselves.
pub fn create_response(instructions: Option<&str>, audio_only: bool) -> Value {
    let mut resp = json!({});
    if let Some(instr) = instructions {
        resp["instructions"] = Value::String(instr.to_string());
    }
    if audio_only {
        resp["output_modalities"] = json!(["audio"]);
    }
    json!({ "type": "response.create", "response": resp })
}

pub fn cancel_response() -> Value {
    json!({ "type": "response.cancel" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_limits_are_bounded_and_use_retention_ratio() {
        let update = session_update(
            "hello",
            &[],
            None,
            &VadParams::default(),
            Some(&SessionLimits {
                max_output_tokens: 10_000,
                context_token_limit: 8_000,
                retention_ratio: 0.8,
            }),
        );
        assert_eq!(update.pointer("/session/max_output_tokens"), Some(&json!(4096)));
        assert_eq!(
            update.pointer("/session/truncation/token_limits/post_instructions"),
            Some(&json!(8000))
        );
    }
}
