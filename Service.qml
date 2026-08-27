import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

Item {
  id: root

  property var shell: null
  property var pluginRegistry: null

  // Where this plugin was cloned to — normally ~/.config/omarchy/plugins/nawaf.perla.
  // bin/perla-setup lives inside it, so the setup button never fetches anything
  // the user has not already accepted by adding the plugin.
  readonly property string pluginDir: String(Qt.resolvedUrl("."))
    .replace(/^file:\/\//, "")
    .replace(/\/$/, "")

  // `omarchy plugin add` copies files and runs nothing, so on a fresh install
  // the daemon is simply absent. Probe for it instead of letting every button
  // fail silently against a binary that is not there.
  property bool installProbed: false
  property string daemonPath: ""
  readonly property bool installed: daemonPath !== ""
  // Prefer the absolute path we found: ~/.local/bin is not always on the
  // PATH that omarchy-shell inherited at login.
  readonly property string daemonBin: daemonPath !== "" ? daemonPath : "perla-d"

  readonly property string runtimeDir: String(Quickshell.env("XDG_RUNTIME_DIR") || "")
  readonly property string userName: Quickshell.env("USER") || Quickshell.env("LOGNAME") || "user"
  readonly property string stateDir: runtimeDir !== "" ? runtimeDir + "/perla" : "/tmp/perla-" + userName
  readonly property string statePath: stateDir + "/state.json"
  readonly property string harnessDir: runtimeDir !== "" ? runtimeDir + "/omarchy-harness" : ""
  readonly property string harnessPath: harnessDir !== "" ? harnessDir + "/state.json" : ""

  property string status: "disconnected"
  property string speaker: "idle"
  property bool muted: false
  property bool reconnecting: false
  property var error: null
  property var phase: null
  property var activity: null
  property real mic_level: 0.0
  property int held_updates: 0
  property real session_usd: 0.0
  property var last_transcript: null
  property int pid: 0
  property string provider: "openai"
  property string model: "gpt-realtime-2.1-mini"
  property string progress_mode: "off"
  property bool has_openai_key: false
  property bool has_grok_key: false
  property bool has_key: false
  property bool start_muted: false
  property string voice: "marin"
  property var voice_language: null

  // Settings can contain API keys. Keep them off the command line, where
  // another process owned by the same user could briefly read them via /proc.
  property var settingsQueue: []
  property string pendingSettingsBody: ""
  property var messageQueue: []
  property string pendingMessageBody: ""

  property bool daemonDriving: false
  property bool harnessDriving: false
  readonly property bool driving: daemonDriving || harnessDriving

  readonly property bool connected: Model.isConnected(root)
  readonly property bool listening: Model.isListening(root)
  readonly property bool speaking: Model.isSpeaking(root)
  readonly property bool working: Model.isWorking(root)
  readonly property real orbLevel: mic_level

  function applyState(raw) {
    var parsed = Model.parseState(raw)
    status = parsed.status
    speaker = parsed.speaker
    muted = parsed.muted
    reconnecting = parsed.reconnecting
    error = parsed.error
    phase = parsed.phase
    activity = parsed.activity
    mic_level = parsed.mic_level
    held_updates = parsed.held_updates
    session_usd = parsed.session_usd
    last_transcript = parsed.last_transcript
    pid = parsed.pid
    daemonDriving = parsed.driving === true
    provider = parsed.provider
    model = parsed.model
    progress_mode = parsed.progress_mode
    has_openai_key = parsed.has_openai_key === true
    has_grok_key = parsed.has_grok_key === true
    has_key = parsed.has_key === true
    start_muted = parsed.start_muted === true
    voice = parsed.voice || "marin"
    voice_language = parsed.voice_language
  }

  function applyHarness(raw) {
    harnessDriving = Model.parseHarness(raw)
  }

  function applyDefaults() {
    applyState("")
  }

  function runDaemon(args) {
    if (!installed) return
    var cmd = [daemonBin]
    for (var i = 0; i < args.length; i++) cmd.push(args[i])
    Quickshell.execDetached(cmd)
  }

  function probeInstall() {
    if (!probeProcess.running) probeProcess.running = true
  }

  function applyProbe(raw) {
    daemonPath = String(raw || "").replace(/^\s+|\s+$/g, "")
    installProbed = true
  }

  function shellQuote(value) {
    return "'" + String(value).replace(/'/g, "'\\''") + "'"
  }

  // Setup and removal run in a visible terminal rather than silently inside the
  // shell process: the scripts need sudo for missing packages, and watching the
  // thing that touches your system is the whole point.
  function runInTerminal(command) {
    Quickshell.execDetached(["omarchy-launch-floating-terminal-with-presentation", command])
  }

  function runSetup(withComputerUse) {
    var cmd = shellQuote(pluginDir + "/bin/perla-setup") + " --yes"
    if (withComputerUse === true) cmd += " --with-computer-use"
    runInTerminal(cmd)
  }

  function runUninstall() {
    runInTerminal(shellQuote(pluginDir + "/bin/perla-uninstall"))
  }

  function start() { runDaemon(["start"]) }
  function stop() { runDaemon(["stop"]) }
  function toggleListen() { runDaemon(["toggle-listen"]) }
  function mute() { runDaemon(["mute"]) }

  function sendText(t) {
    var text = String(t || "").replace(/^\s+|\s+$/g, "")
    if (text === "") return
    messageQueue.push(text)
    pumpMessages()
  }

  function pumpMessages() {
    if (!installed || messageProcess.running || messageQueue.length === 0) return
    pendingMessageBody = messageQueue.shift()
    messageProcess.running = true
  }

  function saveSettings(patch) {
    settingsQueue.push(JSON.stringify(patch || {}))
    pumpSettings()
  }

  function pumpSettings() {
    if (!installed || settingsProcess.running || settingsQueue.length === 0) return
    pendingSettingsBody = settingsQueue.shift()
    settingsProcess.running = true
  }

  function setProvider(name) {
    saveSettings({ provider: String(name || "openai") })
  }

  function setModel(name) {
    saveSettings({ model: String(name || "gpt-realtime-2.1-mini") })
  }

  function setProgressMode(mode) {
    saveSettings({ progress_mode: String(mode || "off") })
  }

  function setStartMuted(on) {
    saveSettings({ start_muted: on === true })
  }

  // "auto" clears the pin daemon-side; the dropdown never sends null.
  function setVoiceLanguage(code) {
    saveSettings({ voice_language: String(code || "auto") })
  }

  // The daemon owns the trail and the clipboard call — the panel only asks.
  function copyDebugLog() {
    runDaemon(["log", "--copy"])
  }

  function saveKeys(openaiKey, grokKey) {
    var patch = {}
    var open = String(openaiKey || "").replace(/^\s+|\s+$/g, "")
    var grok = String(grokKey || "").replace(/^\s+|\s+$/g, "")
    if (open !== "") patch.openai_key = open
    if (grok !== "") patch.grok_key = grok
    if (Object.keys(patch).length === 0) return
    saveSettings(patch)
  }

  // Answers "is perla-d actually on this machine". ~/.local/bin first, because
  // that is where setup puts it and it is not always on the shell's PATH.
  Process {
    id: probeProcess
    command: ["sh", "-c",
      "if [ -x \"$HOME/.local/bin/perla-d\" ]; then printf %s \"$HOME/.local/bin/perla-d\"; else command -v perla-d 2>/dev/null | tr -d '\\n'; fi"]
    stdout: StdioCollector {
      onStreamFinished: root.applyProbe(text)
    }
    onExited: function(exitCode, exitStatus) {
      // An empty stream still means "answered": nothing is installed.
      root.installProbed = true
    }
  }

  // Keep asking until it appears, so the panel flips over on its own the moment
  // the setup terminal finishes. Stops as soon as the answer is yes.
  Timer {
    interval: 3000
    repeat: true
    running: !root.installed
    triggeredOnStart: true
    onTriggered: root.probeInstall()
  }

  Process {
    id: settingsProcess
    command: [root.daemonBin, "set-config", "--stdin"]
    stdinEnabled: true

    onStarted: {
      write(root.pendingSettingsBody + "\n")
      root.pendingSettingsBody = ""
    }

    onExited: function(exitCode, exitStatus) {
      root.pendingSettingsBody = ""
      Qt.callLater(root.pumpSettings)
    }
  }

  Process {
    id: messageProcess
    command: [root.daemonBin, "send", "--stdin"]
    stdinEnabled: true

    onStarted: {
      write(root.pendingMessageBody + "\n")
      root.pendingMessageBody = ""
    }

    onExited: function(exitCode, exitStatus) {
      root.pendingMessageBody = ""
      Qt.callLater(root.pumpMessages)
    }
  }

  FileView {
    id: stateFile
    path: root.statePath
    watchChanges: true
    atomicWrites: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.applyState(text())
    onLoadFailed: root.applyDefaults()
  }

  FileView {
    path: root.stateDir
    watchChanges: true
    printErrors: false
    onFileChanged: stateFile.reload()
  }

  FileView {
    id: harnessFile
    path: root.harnessPath
    watchChanges: true
    atomicWrites: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.applyHarness(text())
    onLoadFailed: root.harnessDriving = false
  }

  FileView {
    path: root.harnessDir
    watchChanges: true
    printErrors: false
    onFileChanged: harnessFile.reload()
  }

  // FileView cannot watch a path that does not exist yet, so keep probing
  // until the daemon writes a pid. watchChanges takes over after that.
  Timer {
    interval: 2000
    repeat: true
    running: root.installed && root.pid <= 0
    triggeredOnStart: true
    onTriggered: {
      stateFile.reload()
      if (root.harnessPath !== "") harnessFile.reload()
    }
  }
}
