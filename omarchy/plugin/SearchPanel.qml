import Quickshell
import Quickshell.Wayland
import QtQuick
import qs.Commons

// The plugin's panel entry point. The shell loads it once (keepLoaded) with no bar and no widget to hang from, and
// calls open() and close() for the hotkey, `recall toggle` and `recall settings`. It owns the daemon connection and
// a full-screen layer-shell window; what is drawn inside is PanelCard.
Item {
  id: root
  property var shell: null
  property var manifest: null
  property string omarchyPath: ""
  // The shell reads this to decide whether the hotkey should open or close the panel.
  property bool opened: false

  function open(payload) {
    var options = ({})
    try {
      options = JSON.parse(payload || "{}") || ({})
    } catch (error) {
      // A payload that isn't JSON just opens the search view.
    }
    opened = true
    card.present(options)
  }
  // Also what the hotkey calls on an open panel. With unsaved settings the card asks first and the panel stays up
  // until the answer, so `opened` only turns false once the card says it may close.
  function close() {
    card.requestClose()
  }

  Backend {
    id: daemon
    watching: root.opened
  }

  // A window of its own rather than the shell's KeyboardPanel: that one needs the bar and a bar widget to grow
  // from, and this panel is summoned by a hotkey whether or not its widget is on the bar.
  PanelWindow {
    visible: root.opened
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore
    WlrLayershell.namespace: "recall-for-omarchy"
    WlrLayershell.layer: WlrLayer.Overlay
    // The panel is typed into from the moment it appears, so it takes the keyboard outright.
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive
    RecallTheme { id: theme }
    // Dims the desktop with the theme's own scrim, which is light on a light theme.
    Rectangle {
      anchors.fill: parent
      color: theme.scrim
      MouseArea { anchors.fill: parent; onClicked: card.requestClose() }
    }
    PanelCard {
      id: card
      anchors.centerIn: parent
      width: Math.min(Style.space(880), parent.width - Style.space(40))
      height: Math.min(Style.space(650), parent.height - Style.space(40))
      backend: daemon
      active: root.opened
      onCloseRequested: {
        root.opened = false
        // Lets the daemon drop the search model from memory once nothing is searching.
        daemon.send("release")
      }
    }
  }
}
