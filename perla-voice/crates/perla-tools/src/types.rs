use serde_json::{json, Map, Value};

/// One tool the realtime model may call. `parameters` is a JSON schema.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

impl ToolDef {
    /// Realtime tools use the "function" envelope.
    pub fn openai_shape(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        })
    }

    /// Gemini Bidi functionDeclarations shape.
    pub fn gemini_shape(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        })
    }
}

/// Result of a tool call, sent back to the model as `function_call_output`.
///
/// Keys prefixed `__` are side-channel data for the engine (e.g. an image to
/// attach) — never serialized into the function output.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    pub payload: Map<String, Value>,
}

impl ToolResult {
    pub fn success(payload: Value) -> Self {
        Self {
            ok: true,
            payload: into_map(payload),
        }
    }

    pub fn failure(payload: Value) -> Self {
        Self {
            ok: false,
            payload: into_map(payload),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::failure(json!({ "error": message.into() }))
    }

    pub fn public_payload(&self) -> Map<String, Value> {
        self.payload
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn output_json(&self) -> String {
        let mut d = self.public_payload();
        d.insert("ok".into(), Value::Bool(self.ok));
        serde_json::to_string(&Value::Object(d)).unwrap_or_else(|_| "{}".into())
    }

    /// Convenience: the `status` field, when present ("submitted" / "queued" / ...).
    pub fn status(&self) -> Option<&str> {
        self.payload.get("status").and_then(|v| v.as_str())
    }
}

fn into_map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    }
}
