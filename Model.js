// Pure helpers for the Perla bar face. No Qt, so the same functions can run
// under node. The daemon owns state.json; this module only parses it.

var STATUSES = ["disconnected", "connecting", "connected", "tool_running", "error"]
var SPEAKERS = ["idle", "user", "model"]
var ROLES = ["user", "assistant", "tool"]

function defaultState() {
  return {
    status: "disconnected",
    speaker: "idle",
    muted: false,
    reconnecting: false,
    error: null,
    phase: null,
    activity: null,
    mic_level: 0.0,
    held_updates: 0,
    session_usd: 0.0,
    driving: false,
    last_transcript: null,
    pid: 0,
    provider: "openai",
    model: "gpt-realtime-2.1-mini",
    progress_mode: "off",
    has_openai_key: false,
    has_grok_key: false,
    has_gemini_key: false,
    has_key: false,
    start_muted: false,
    voice: "marin",
    voice_language: null
  }
}

// The languages the settings panel offers, mirroring the macOS app's
// "Voice language" picker. `value` is the code stored in config.toml;
// "auto" is the sentinel meaning "no pin".
var VOICE_LANGUAGES = [
  { value: "auto", label: "Auto (match the speaker)" },
  { value: "en", label: "English" },
  { value: "ar", label: "\u0627\u0644\u0639\u0631\u0628\u064a\u0629 \u00b7 Arabic" },
  { value: "fr", label: "Fran\u00e7ais \u00b7 French" },
  { value: "es", label: "Espa\u00f1ol \u00b7 Spanish" },
  { value: "de", label: "Deutsch \u00b7 German" }
]

var OPENAI_MODELS = [
  { value: "gpt-realtime-2.1-mini", label: "2.1 Mini · efficient" },
  { value: "gpt-realtime-2.1", label: "2.1 · best quality" },
  { value: "gpt-realtime-1.5", label: "1.5 · natural voice" }
]

var GROK_MODELS = [
  { value: "grok-4-fast-realtime", label: "Grok 4 Fast Realtime" }
]

var GEMINI_MODELS = [
  { value: "models/gemini-2.0-flash-exp", label: "Gemini 2.0 Flash (Free)" },
  { value: "models/gemini-2.0-flash", label: "Gemini 2.0 Flash GA (Free)" }
]

var GEMINI_VOICES = [
  { value: "Puck", label: "Puck" },
  { value: "Charon", label: "Charon" },
  { value: "Kore", label: "Kore" },
  { value: "Fenrir", label: "Fenrir" },
  { value: "Aoede", label: "Aoede" }
]

var OPENAI_VOICES = [
  { value: "marin", label: "Marin" },
  { value: "alloy", label: "Alloy" },
  { value: "ash", label: "Ash" },
  { value: "ballad", label: "Ballad" },
  { value: "coral", label: "Coral" },
  { value: "echo", label: "Echo" },
  { value: "sage", label: "Sage" },
  { value: "shimmer", label: "Shimmer" },
  { value: "verse", label: "Verse" }
]
function realtimeModelOptions(perla) {
  var provider = perla && perla.provider === "gemini"
    ? "gemini"
    : (perla && perla.provider === "grok" ? "grok" : "openai")
  var base = provider === "gemini"
    ? GEMINI_MODELS
    : (provider === "grok" ? GROK_MODELS : OPENAI_MODELS)
  var current = perla && perla.model ? String(perla.model) : ""
  var result = []
  var known = false
  for (var i = 0; i < base.length; i++) {
    result.push(base[i])
    if (base[i].value === current) known = true
  }
  // Preserve a hand-edited/snapshot model instead of blanking the drawer.
  if (current !== "" && !known) result.unshift({ value: current, label: "Custom · " + current })
  return result
}

function realtimeModelValue(perla) {
  if (perla && perla.model) return String(perla.model)
  if (perla && perla.provider === "gemini") return "models/gemini-2.0-flash-exp"
  return perla && perla.provider === "grok"
    ? "grok-4-fast-realtime"
    : "gpt-realtime-2.1-mini"
}

function voiceOptions(perla) {
  return perla && perla.provider === "gemini" ? GEMINI_VOICES : OPENAI_VOICES
}

function voiceValue(perla) {
  if (perla && perla.voice) return String(perla.voice)
  return perla && perla.provider === "gemini" ? "Puck" : "marin"
}

function progressModeOptions() {
  return [
    { value: "off", label: "Completions only · lowest cost" },
    { value: "big", label: "Major milestones" },
    { value: "steps", label: "Every step · highest cost" }
  ]
}

function progressModeValue(perla) {
  return oneOf(perla && perla.progress_mode, ["off", "big", "steps"], "off")
}

function voiceLanguageOptions() {
  return VOICE_LANGUAGES
}

/// The dropdown's current selection: the stored code, or "auto" when unset or
/// unrecognised (a hand-edited config.toml should not blank the control).
function voiceLanguageValue(perla) {
  var code = perla && perla.voice_language ? String(perla.voice_language).toLowerCase() : ""
  if (code === "") return "auto"
  for (var i = 0; i < VOICE_LANGUAGES.length; i++) {
    if (VOICE_LANGUAGES[i].value === code) return code
  }
  return "auto"
}

function oneOf(value, allowed, fallback) {
  var s = String(value === undefined || value === null ? "" : value)
  for (var i = 0; i < allowed.length; i++) if (allowed[i] === s) return s
  return fallback
}

function asNumber(value, fallback) {
  var n = Number(value)
  return isFinite(n) ? n : fallback
}

function asInt(value, fallback) {
  var n = parseInt(value, 10)
  return isFinite(n) ? n : fallback
}

function asNullableString(value) {
  if (value === undefined || value === null) return null
  var s = String(value)
  return s === "" ? null : s
}

function parseTranscript(value) {
  if (!value || typeof value !== "object") return null
  var role = oneOf(value.role, ROLES, "")
  var text = String(value.text === undefined || value.text === null ? "" : value.text)
  if (role === "" && text === "") return null
  return { role: role || "assistant", text: text }
}

function parseState(jsonText) {
  var fallback = defaultState()
  var raw = String(jsonText === undefined || jsonText === null ? "" : jsonText)
  if (raw === "") return fallback
  try {
    var data = JSON.parse(raw)
    if (!data || typeof data !== "object") return fallback
    var provider = oneOf(data.provider, ["openai", "grok", "gemini"], fallback.provider)
    return {
      status: oneOf(data.status, STATUSES, fallback.status),
      speaker: oneOf(data.speaker, SPEAKERS, fallback.speaker),
      muted: data.muted === true,
      reconnecting: data.reconnecting === true,
      error: asNullableString(data.error),
      phase: asNullableString(data.phase),
      activity: asNullableString(data.activity),
      mic_level: Math.max(0, Math.min(1, asNumber(data.mic_level, fallback.mic_level))),
      held_updates: Math.max(0, asInt(data.held_updates, fallback.held_updates)),
      session_usd: Math.max(0, asNumber(data.session_usd, fallback.session_usd)),
      driving: data.driving === true,
      last_transcript: parseTranscript(data.last_transcript),
      pid: Math.max(0, asInt(data.pid, fallback.pid)),
      provider: provider,
      model: asNullableString(data.model) || (provider === "gemini"
        ? "models/gemini-2.0-flash-exp"
        : (provider === "grok" ? "grok-4-fast-realtime" : fallback.model)),
      progress_mode: oneOf(data.progress_mode, ["off", "big", "steps"], fallback.progress_mode),
      has_openai_key: data.has_openai_key === true,
      has_grok_key: data.has_grok_key === true,
      has_gemini_key: data.has_gemini_key === true,
      has_key: data.has_key === true,
      start_muted: data.start_muted === true,
      voice: asNullableString(data.voice) || (provider === "gemini" ? "Puck" : fallback.voice),
      voice_language: asNullableString(data.voice_language)
    }
  } catch (e) {
    return fallback
  }
}

function parseHarness(jsonText) {
  try {
    var data = JSON.parse(String(jsonText === undefined || jsonText === null ? "" : jsonText))
    return !!(data && data.driving === true)
  } catch (e) {
    return false
  }
}

function isConnected(state) {
  if (!state) return false
  return state.status === "connected" || state.status === "tool_running"
}

function isListening(state) {
  return !!(isConnected(state) && !state.muted && state.speaker)
}

function isSpeaking(state) {
  return !!(state && state.speaker === "model")
}

function isWorking(state) {
  return !!(state && state.status === "tool_running")
}

// `omarchy plugin add` copies files and runs nothing, so the daemon is not
// there yet the first time the plugin loads. The panel asks the shell whether
// perla-d exists and reports the answer back through these two properties;
// until the probe has answered once we say nothing rather than flashing a
// setup card at someone who is already installed.
function needsSetup(state) {
  return !!(state && state.installProbed === true && state.installed !== true)
}

// What the setup button is about to do, in the panel's own words. Kept here so
// the disclosure and the script's banner can be checked against each other.
function setupSummary() {
  return [
    "Installs the perla-d daemon into ~/.local/bin",
    "Enables perla.service for your user",
    "Installs missing Arch packages — asks for your password",
    "Opens a terminal so you can watch and cancel"
  ]
}

function settingsHint(state) {
  if (!state) return "Add an API key in Settings."
  if (!state.has_key) {
    if (state.provider === "gemini") return "Add a Gemini API key to talk (free at aistudio.google.com)."
    return state.provider === "grok"
      ? "Add an xAI API key to talk."
      : "Add an OpenAI API key to talk."
  }
  return ""
}

function maskKeyHint(hasKey) {
  return hasKey ? "Key saved — paste a new one to replace" : "Paste API key and Save"
}

function statusLabel(state) {
  if (!state) return "Disconnected"
  if (needsSetup(state)) return "Setup needed"
  if (state.driving) return "Driving"
  if (state.status === "error") return state.error || "Error"
  if (!state.has_key && (state.status === "disconnected" || state.status === "error"))
    return settingsHint(state)
  if (state.status === "disconnected") return "Disconnected"
  if (state.status === "connecting") return state.reconnecting ? "Reconnecting" : "Connecting"
  if (state.reconnecting) return "Reconnecting"
  if (state.status === "tool_running") return state.activity || "Working"
  if (state.muted) return "Muted"
  if (state.speaker === "model") return "Speaking"
  if (state.speaker === "user") return "Listening"
  if (state.status === "connected") return "Connected"
  return "Perla"
}

function orbColorKey(state) {
  if (!state) return "dim"
  if (state.driving) return "urgent"
  if (state.speaker === "model") return "accent"
  if (isListening(state)) return "foreground"
  return "dim"
}

function micGlyph(state) {
  if (!state) return "󰍭"
  if (state.muted || state.status === "disconnected" || state.status === "error") return "󰍭"
  return "󰍬"
}

function transcriptLabel(state) {
  if (!state || !state.last_transcript) return ""
  var line = state.last_transcript
  var role = line.role === "user" ? "You" : (line.role === "tool" ? "Tool" : "Perla")
  var text = String(line.text || "")
  if (text === "") return ""
  return role + ": " + text
}

function sessionCostLabel(usd) {
  var n = asNumber(usd, 0)
  if (!(n > 0)) return ""
  return "Session $" + (n < 0.01 ? n.toFixed(4) : n.toFixed(2))
}

if (typeof module !== "undefined") {
  module.exports = {
    parseState: parseState,
    parseHarness: parseHarness,
    settingsHint: settingsHint,
    needsSetup: needsSetup,
    setupSummary: setupSummary,
    maskKeyHint: maskKeyHint,
    statusLabel: statusLabel,
    orbColorKey: orbColorKey,
    isConnected: isConnected,
    isListening: isListening,
    isSpeaking: isSpeaking,
    isWorking: isWorking,
    realtimeModelOptions: realtimeModelOptions,
    realtimeModelValue: realtimeModelValue,
    voiceOptions: voiceOptions,
    voiceValue: voiceValue,
    GEMINI_MODELS: GEMINI_MODELS,
    GEMINI_VOICES: GEMINI_VOICES,
    OPENAI_VOICES: OPENAI_VOICES,
    progressModeOptions: progressModeOptions,
    progressModeValue: progressModeValue,
    sessionCostLabel: sessionCostLabel
  }
}
