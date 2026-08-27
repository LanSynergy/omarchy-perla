import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "Model.js" as Model

Panel {
  id: root
  moduleName: "nawaf.perla"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  /// Left-click opens the short version, right-click the whole thing. Clicking
  /// the chip used to toggle listening, which made a single misclick change
  /// state — a menu never does.
  property bool compact: false
  readonly property var barIdentity: hostWidget || root

  readonly property var perla: bar && bar.shell ? bar.shell.serviceFor("nawaf.perla") : null
  readonly property color contentForeground: bar ? bar.foreground : Color.foreground
  readonly property string contentFontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property color dim: Qt.darker(contentForeground, 1.55)
  readonly property string statusText: Model.statusLabel(perla)
  readonly property string transcriptText: Model.transcriptLabel(perla)
  readonly property string activityText: perla && perla.activity ? String(perla.activity) : ""
  readonly property string costText: Model.sessionCostLabel(perla ? perla.session_usd : 0)
  readonly property string heldText: perla && perla.held_updates > 0
    ? (perla.held_updates + (perla.held_updates === 1 ? " update waiting" : " updates waiting"))
    : ""
  readonly property string keyHint: Model.settingsHint(perla)
  // Until the daemon exists the rest of the panel has nothing to control, so
  // the setup card replaces it rather than sitting above a wall of dead buttons.
  readonly property bool setupNeeded: Model.needsSetup(perla)
  property bool setupComputerUse: false
  property bool setupLaunched: false
  readonly property bool settingsEditing: openaiKey.activeFocus || grokKey.activeFocus
    || sendField.activeFocus || providerBox.popupOpen || modelBox.popupOpen
    || progressBox.popupOpen || languageBox.popupOpen

  function open() {
    if (root.perla) root.perla.probeInstall()
    root.controller.show()
    Qt.callLater(function() {
      if (root.opened) setCenterHoverRevealSuppressed(true)
    })
  }

  function close() {
    setCenterHoverRevealSuppressed(false)
    root.controller.hide()
  }

  function toggle() {
    if (root.opened) root.close()
    else root.open()
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }

  function sendFromField() {
    if (!perla) return
    perla.sendText(sendField.text)
    sendField.text = ""
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(340))
    contentHeight: panel.fittedContentHeight(bodyColumn.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      blocked: root.settingsEditing
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }

      Flickable {
        id: bodyScroll
        anchors.fill: parent
        contentWidth: width
        contentHeight: bodyColumn.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        interactive: contentHeight > height

        Column {
          id: bodyColumn
          width: bodyScroll.width
          spacing: Style.space(12)

          Column {
            width: parent.width
            spacing: Style.space(2)

            Row {
              width: parent.width
              spacing: Style.space(2)

              Image {
                source: Qt.resolvedUrl("assets/perla-logo.png")
                sourceSize.width: 96
                sourceSize.height: 96
                width: Style.font.title + Style.space(2)
                height: width
                smooth: true
                anchors.verticalCenter: parent.verticalCenter
                // The mark carries the state the header text spells out.
                opacity: root.perla && root.perla.connected ? 1.0 : 0.45
                Behavior on opacity { NumberAnimation { duration: 160 } }
              }

              Text {
                text: "Perla"
                color: root.contentForeground
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.title
                font.bold: true
                anchors.verticalCenter: parent.verticalCenter
              }
            }

            Text {
              width: parent.width
              text: root.statusText
              color: root.perla && root.perla.driving ? Color.urgent : root.dim
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }
          }

          Column {
            visible: root.setupNeeded
            width: parent.width
            spacing: Style.space(10)

            Text {
              width: parent.width
              text: "Adding a plugin copies files and runs nothing — that is Omarchy keeping you safe. Perla also needs a voice daemon, so one more step installs it."
              color: root.contentForeground
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }

            Column {
              width: parent.width
              spacing: Style.space(2)

              Repeater {
                model: Model.setupSummary()

                Text {
                  width: parent.width
                  text: "\u00b7  " + modelData
                  color: root.dim
                  font.family: root.contentFontFamily
                  font.pixelSize: Style.font.caption
                  wrapMode: Text.WordWrap
                }
              }
            }

            Toggle {
              width: parent.width
              label: "Computer use"
              description: "Also let Perla see the screen, click, and type"
              checked: root.setupComputerUse
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: root.setupComputerUse = !root.setupComputerUse
            }

            Button {
              text: root.setupLaunched ? "Setup is running…" : "Set up Perla"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: !!root.perla
              opacity: enabled ? 1.0 : 0.4
              onClicked: {
                if (!root.perla) return
                root.perla.runSetup(root.setupComputerUse)
                root.setupLaunched = true
              }
            }

            Text {
              visible: root.setupLaunched
              width: parent.width
              text: "A terminal opened so you can watch it. Perla wakes up here on her own when it finishes."
              color: root.dim
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }

          Text {
            visible: !root.setupNeeded && root.transcriptText !== ""
            width: parent.width
            text: root.transcriptText
            color: root.contentForeground
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.body
            wrapMode: Text.WordWrap
          }

          Text {
            visible: !root.setupNeeded && root.activityText !== ""
            width: parent.width
            text: root.activityText
            color: root.dim
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }

          Text {
            visible: !root.setupNeeded && (root.costText !== "" || root.heldText !== "")
            width: parent.width
            text: root.heldText !== "" && root.costText !== ""
              ? root.heldText + " · " + root.costText
              : (root.heldText !== "" ? root.heldText : root.costText)
            color: root.dim
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.caption
          }

          Row {
            visible: !root.setupNeeded
            spacing: Style.space(6)

            Button {
              text: "Start"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: !!(root.perla && !root.perla.connected)
              opacity: enabled ? 1.0 : 0.4
              onClicked: if (root.perla) root.perla.start()
            }

            Button {
              text: "Stop"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: !!(root.perla && root.perla.status !== "disconnected")
              opacity: enabled ? 1.0 : 0.4
              onClicked: if (root.perla) root.perla.stop()
            }

            Button {
              text: "Mute"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: !!(root.perla && root.perla.connected)
              opacity: enabled ? 1.0 : 0.4
              onClicked: if (root.perla) root.perla.mute()
            }
          }

          Button {
            visible: !root.setupNeeded && root.compact
            text: "Settings…"
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
            onClicked: root.compact = false
          }

          Row {
            visible: !root.setupNeeded && !root.compact
            width: parent.width
            spacing: Style.space(6)

            TextField {
              id: sendField
              width: parent.width - sendButton.implicitWidth - parent.spacing
              placeholderText: "Send text"
              foreground: root.contentForeground
              enabled: !!(root.perla && root.perla.connected)
              onAccepted: root.sendFromField()
              Keys.onEscapePressed: root.close()
            }

            Button {
              id: sendButton
              text: "Send"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: sendField.enabled && sendField.text !== ""
              opacity: enabled ? 1.0 : 0.4
              onClicked: root.sendFromField()
            }
          }

          Column {
            // Everything below the fold: only the full panel shows it.
            visible: !root.setupNeeded && !root.compact
            width: parent.width
            spacing: Style.space(12)

            PanelSeparator {
              width: parent.width
            }

            PanelSectionHeader {
              text: "SETTINGS"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
            }

            Text {
              visible: root.keyHint !== ""
              width: parent.width
              text: root.keyHint
              color: Color.urgent
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }

            Toggle {
              width: parent.width
              label: "Voice"
              description: "Connect the realtime session"
              checked: !!(root.perla && root.perla.connected)
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: {
                if (!root.perla) return
                if (root.perla.connected) root.perla.stop()
                else root.perla.start()
              }
            }

            Toggle {
              width: parent.width
              label: "Mute"
              description: "Keep listening off until you unmute"
              checked: !!(root.perla && root.perla.muted)
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: if (root.perla) root.perla.mute()
            }

            Toggle {
              width: parent.width
              label: "Start muted"
              description: "Join the next session with the mic off"
              checked: !!(root.perla && root.perla.start_muted)
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: if (root.perla) root.perla.setStartMuted(!(root.perla.start_muted === true))
            }

            Dropdown {
              id: providerBox
              width: parent.width
              label: "Provider"
              value: root.perla ? String(root.perla.provider || "openai") : "openai"
              options: [
                { value: "openai", label: "OpenAI" },
                { value: "grok", label: "Grok (xAI)" }
              ]
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onChanged: function(value) {
                if (root.perla && value !== root.perla.provider) root.perla.setProvider(value)
              }
            }

            Dropdown {
              id: modelBox
              width: parent.width
              label: "Voice model"
              value: Model.realtimeModelValue(root.perla)
              options: Model.realtimeModelOptions(root.perla)
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onChanged: function(value) {
                if (root.perla && value !== Model.realtimeModelValue(root.perla)) {
                  root.perla.setModel(value)
                }
              }
            }

            Text {
              width: parent.width
              text: root.perla && root.perla.provider === "openai"
                ? "Mini is the production default: faster and much cheaper. Model changes restart the voice session."
                : "Model changes restart the voice session."
              color: root.dim
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }

            Dropdown {
              id: progressBox
              width: parent.width
              label: "Spoken progress"
              value: Model.progressModeValue(root.perla)
              options: Model.progressModeOptions()
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onChanged: function(value) {
                if (root.perla && value !== Model.progressModeValue(root.perla)) {
                  root.perla.setProgressMode(value)
                }
              }
            }

            Dropdown {
              id: languageBox
              width: parent.width
              label: "Language"
              value: Model.voiceLanguageValue(root.perla)
              options: Model.voiceLanguageOptions()
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onChanged: function(value) {
                if (root.perla && value !== Model.voiceLanguageValue(root.perla)) {
                  root.perla.setVoiceLanguage(value)
                }
              }
            }

            Column {
              width: parent.width
              spacing: Style.space(4)

              Text {
                text: "OPENAI KEY"
                color: root.dim
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.caption
                font.bold: true
              }
              TextField {
                id: openaiKey
                width: parent.width
                password: true
                placeholderText: Model.maskKeyHint(root.perla && root.perla.has_openai_key)
                foreground: root.contentForeground
                Keys.onEscapePressed: root.close()
              }
            }

            Column {
              width: parent.width
              spacing: Style.space(4)

              Text {
                text: "GROK KEY"
                color: root.dim
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.caption
                font.bold: true
              }
              TextField {
                id: grokKey
                width: parent.width
                password: true
                placeholderText: Model.maskKeyHint(root.perla && root.perla.has_grok_key)
                foreground: root.contentForeground
                Keys.onEscapePressed: root.close()
              }
            }

            Button {
              // The mirror of the setup button: same script directory, same
              // visible terminal, nothing removed without the user watching.
              text: "Uninstall the daemon\u2026"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: if (root.perla) root.perla.runUninstall()
            }

            Button {
              text: "Copy debug log"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              onClicked: if (root.perla) root.perla.copyDebugLog()
            }

            Button {
              text: "Save keys"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
              enabled: openaiKey.text !== "" || grokKey.text !== ""
              opacity: enabled ? 1.0 : 0.4
              onClicked: {
                if (!root.perla) return
                root.perla.saveKeys(openaiKey.text, grokKey.text)
                openaiKey.text = ""
                grokKey.text = ""
              }
            }
          }
        }
      }
    }
  }
}
