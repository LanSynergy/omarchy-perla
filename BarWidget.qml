import QtQuick
import qs.Ui
import qs.Commons
import "Model.js" as Model

BarWidget {
  id: root
  moduleName: "nawaf.perla"

  readonly property var perla: bar && bar.shell ? bar.shell.serviceFor("nawaf.perla") : null
  readonly property real orbLevel: perla ? Number(perla.orbLevel || 0) : 0
  readonly property string orbKey: Model.orbColorKey(perla)
  readonly property color orbColor: {
    if (orbKey === "urgent") return Color.urgent
    if (orbKey === "accent") return Color.accent
    if (orbKey === "foreground") return bar ? bar.barForeground : Color.foreground
    return Qt.darker(bar ? bar.barForeground : Color.foreground, 1.6)
  }

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  function togglePanel() {
    if (panelLoader.item && panelLoader.item.toggle) panelLoader.item.toggle()
  }

  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false

  function open() {
    if (panelLoader.item) panelLoader.item.open()
  }

  function close() {
    if (panelLoader.item && panelLoader.item.close) panelLoader.item.close()
  }

  function toggle() {
    if (panelLoader.item && panelLoader.item.toggle) panelLoader.item.toggle()
  }

  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false

  function closeForPopoutSwitch() {
    if (panelLoader.item) panelLoader.item.closeForPopoutSwitch()
  }

  // A click on a bar icon should reveal, not change state. Listening is now
  // started and stopped from a button inside the menu, where the label says
  // what it will do.
  function openMenu(compact) {
    var p = panelLoader.item
    if (!p) return
    if (p.opened && p.compact === compact) {
      p.close()
      return
    }
    p.compact = compact
    p.open()
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    slotSize: Style.bar.statusSlot
    tooltipText: Model.statusLabel(root.perla)
    iconComponent: Component {
      Item {
        // Perla's pearl, the same mark the macOS app uses. It breathes with the
        // mic and dims when she is not listening, so the chip reads as state at
        // a glance instead of needing a legend.
        Image {
          id: orb
          anchors.centerIn: parent
          source: Qt.resolvedUrl("assets/perla-orb.png")
          sourceSize.width: 64
          sourceSize.height: 64
          width: Math.round(parent.width * 0.92)
          height: width
          smooth: true
          // Only shown when the colour means something. At bar size a dimmed
          // pearl under a crossed mic is two shapes fighting for 20 pixels, so
          // the glyph owns the "off" states and the pearl owns the live ones.
          visible: root.orbKey !== "dim"
          opacity: 1.0
          scale: 1.0 + 0.14 * Math.max(0, Math.min(1, root.orbLevel))
          Behavior on opacity { NumberAnimation { duration: 160 } }
          Behavior on scale { NumberAnimation { duration: 90 } }
        }

        // A ring while she is speaking, brighter while she is driving the
        // desktop — the one state worth looking up for.
        Rectangle {
          anchors.centerIn: parent
          width: orb.width + 4
          height: width
          radius: width / 2
          color: "transparent"
          border.width: 1
          border.color: root.orbColor
          visible: root.orbKey === "urgent" || root.orbKey === "accent"
          opacity: root.orbKey === "urgent" ? 0.9 : 0.5
          Behavior on opacity { NumberAnimation { duration: 160 } }
        }

        // A dim pearl alone reads as "asleep", not as "listening is off on
        // purpose", so muted and disconnected keep the crossed mic on top.
        OpticalGlyph {
          anchors.fill: parent
          text: Model.micGlyph(root.perla)
          fontFamily: button.fontFamily
          fontSize: button.fontSize
          color: root.orbColor
          visible: root.orbKey === "dim"
        }
      }
    }

    onPressed: function(b) {
      root.openMenu(b !== Qt.RightButton)
    }
  }
}
