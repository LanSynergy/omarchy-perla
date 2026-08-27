const assert = require("assert")
const Model = require("./Model.js")

const empty = Model.parseState("")
assert.strictEqual(empty.status, "disconnected")
assert.strictEqual(empty.pid, 0)
assert.strictEqual(empty.model, "gpt-realtime-2.1-mini")
assert.strictEqual(Model.progressModeValue(empty), "off")
assert.strictEqual(Model.sessionCostLabel(0.00421), "Session $0.0042")

const live = Model.parseState(JSON.stringify({
  status: "connected",
  speaker: "user",
  muted: false,
  driving: true,
  last_transcript: { role: "user", text: "open the browser" },
  pid: 9
}))
assert.strictEqual(live.status, "connected")
assert.strictEqual(Model.orbColorKey(live), "urgent")
assert.strictEqual(Model.statusLabel(live), "Driving")
assert.ok(Model.parseHarness('{"driving":true}'))
assert.ok(!Model.parseHarness("{"))

const firstRun = Model.parseState("{}")
assert.strictEqual(firstRun.has_key, false)
assert.ok(Model.settingsHint(firstRun).indexOf("OpenAI") >= 0)

const keyed = Model.parseState(JSON.stringify({
  status: "disconnected",
  provider: "grok",
  has_grok_key: true,
  has_key: true
}))
assert.strictEqual(keyed.provider, "grok")
assert.strictEqual(keyed.model, "grok-4-fast-realtime")
assert.strictEqual(Model.settingsHint(keyed), "")

const modelState = Model.parseState(JSON.stringify({
  provider: "openai",
  model: "gpt-realtime-2.1",
  progress_mode: "big"
}))
assert.strictEqual(Model.realtimeModelValue(modelState), "gpt-realtime-2.1")
assert.strictEqual(Model.progressModeValue(modelState), "big")
assert.ok(Model.realtimeModelOptions(modelState).some(function(option) {
  return option.value === "gpt-realtime-2.1-mini"
}))

const customModel = Model.parseState(JSON.stringify({ model: "gpt-realtime-private" }))
assert.strictEqual(Model.realtimeModelOptions(customModel)[0].value, "gpt-realtime-private")
// Setup gating: silence until the probe has answered, so an already-installed
// user never sees the setup card flash on shell start.
const unprobed = Model.parseState("{}")
assert.strictEqual(Model.needsSetup(unprobed), false)
assert.strictEqual(Model.needsSetup(null), false)

const probedMissing = Model.parseState("{}")
probedMissing.installProbed = true
probedMissing.installed = false
assert.strictEqual(Model.needsSetup(probedMissing), true)
assert.strictEqual(Model.statusLabel(probedMissing), "Setup needed")

const probedPresent = Model.parseState("{}")
probedPresent.installProbed = true
probedPresent.installed = true
assert.strictEqual(Model.needsSetup(probedPresent), false)
assert.ok(Model.statusLabel(probedPresent).indexOf("Setup") < 0)

// "Setup needed" outranks the key hint: a key is useless with no daemon.
const drivingButMissing = Model.parseState(JSON.stringify({ status: "connected", driving: true }))
drivingButMissing.installProbed = true
drivingButMissing.installed = false
assert.strictEqual(Model.statusLabel(drivingButMissing), "Setup needed")

assert.ok(Model.setupSummary().length >= 3)
assert.ok(Model.setupSummary().every(function(line) { return typeof line === "string" && line !== "" }))

console.log("ok")
