import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons as Theme

Item {
  id: root
  property var shell: null
  property var manifest: null
  property string omarchyPath: ""
  property bool showSettings: false
  property int settingsSave: -1
  property bool opened: false
  property var hits: []
  property int serial: 0
  property int latestSearch: -1
  property int selected: 0
  property string status: "Starting search service…"
  property string notice: ""
  property bool paused: false
  property bool searching: false

  function send(op, data) {
    var message = data || {}
    message.op = op
    message.id = ++serial
    transport.write(JSON.stringify(message) + "\n")
    return message.id
  }
  function open(payload) {
    opened = true
    showSettings = false
    try {
      var options = JSON.parse(payload || "{}")
      showSettings = options.settings === true
      if (options.query !== undefined) query.text = String(options.query)
    } catch (error) {}
    if (showSettings) send("config")
    else send("prewarm")
    send("status")
    Qt.callLater(function() { if (root.showSettings) settingsForm.forceActiveFocus(); else query.forceActiveFocus() })
  }
  function close() {
    opened = false
    debounce.stop()
    send("release")
  }
  function search() {
    if (!query.text.trim()) { hits = []; searching = false; return }
    searching = true
    latestSearch = send("search", {query: query.text})
  }
  function activate(reveal) {
    if (hits.length && selected >= 0 && selected < hits.length) {
      send(reveal ? "reveal" : "open", {hit: hits[selected]})
      close()
    }
  }
  Process {
    id: transport
    command: [Quickshell.env("HOME") + "/.local/bin/recall-client"]
    running: true
    stdinEnabled: true
    stdout: SplitParser {
      onRead: function(line) {
        try {
          var reply = JSON.parse(line)
          if (reply.error) { root.notice = reply.error; root.searching = false; return }
          if (reply.config !== undefined) settingsForm.load(reply.config)
          if (reply.id === root.settingsSave) { root.showSettings = false; root.notice = "Settings saved" }
          if (reply.line !== undefined) { root.status = reply.line; root.paused = reply.paused }
          if (reply.id === root.latestSearch && !reply.skipped) {
            root.hits = reply.hits || []
            root.selected = 0
            root.searching = false
            root.notice = reply.encoder_error ? "Exact matches · " + reply.encoder_error : reply.meaning ? "Semantic + exact matches" : "Exact matches · semantic encoder warming up"
          }
        } catch (error) { root.notice = "Could not read search response" }
      }
    }
    onExited: { root.notice = "Search connection stopped"; reconnect.restart() }
  }
  Timer { id: reconnect; interval: 2000; onTriggered: transport.running = true }
  Timer { id: debounce; interval: 250; onTriggered: root.search() }
  Timer { interval: 3000; running: root.opened; repeat: true; onTriggered: root.send("status") }
  PanelWindow {
    visible: root.opened
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore
    WlrLayershell.namespace: "recall-for-omarchy"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive
    Rectangle { anchors.fill: parent; color: "#88000000"; MouseArea { anchors.fill: parent; onClicked: root.close() } }
    Rectangle {
      id: panel
      anchors.centerIn: parent
      width: Math.min(880, parent.width - 40)
      height: Math.min(650, parent.height - 40)
      color: Theme.Color.menu.background
      border.color: Qt.alpha(Theme.Color.menu.text, 0.22)
      border.width: 1
      radius: 12
      ColumnLayout {
        anchors.fill: parent
        anchors.margins: 1
        spacing: 0
        RowLayout {
          Layout.fillWidth: true
          Layout.preferredHeight: 60
          Layout.leftMargin: 22
          Layout.rightMargin: 14
          spacing: 10
          RecallIcon { name: "search"; ink: Theme.Color.menu.selectedText; Layout.preferredWidth: 22; Layout.preferredHeight: 22 }
          Text { text: "Recall"; color: Theme.Color.menu.text; font.pixelSize: 18; font.weight: Font.DemiBold; font.family: Theme.Style.fontFamily }
          Text { visible: panel.width >= 500; text: root.showSettings ? "/ Settings" : "/ Search"; color: Qt.alpha(Theme.Color.menu.text, 0.45); font.pixelSize: 12; font.family: Theme.Style.fontFamily }
          Item { Layout.fillWidth: true }
          RecallButton { iconName: root.paused ? "play" : "pause"; compact: true; hint: root.paused ? "Resume indexing" : "Pause indexing"; onClicked: { root.send("pause", {paused: !root.paused}); root.send("status") } }
          RecallButton { iconName: root.showSettings ? "search" : "settings"; compact: true; hint: root.showSettings ? "Back to search" : "Settings"; onClicked: { root.showSettings = !root.showSettings; if (root.showSettings) root.send("config"); else query.forceActiveFocus() } }
          Rectangle { Layout.preferredWidth: 1; Layout.preferredHeight: 18; color: Qt.alpha(Theme.Color.menu.text, 0.12) }
          RecallButton { iconName: "close"; compact: true; hint: "Close · Esc"; onClicked: root.close() }
        }
        Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: Qt.alpha(Theme.Color.menu.text, 0.1) }
        Rectangle {
          visible: !root.showSettings
          Layout.fillWidth: true
          Layout.preferredHeight: 54
          Layout.margins: 18
          radius: 7
          color: Qt.alpha(Theme.Color.menu.text, 0.025)
          border.width: 1
          border.color: query.activeFocus ? Qt.alpha(Theme.Color.menu.selectedText, 0.7) : Qt.alpha(Theme.Color.menu.text, 0.2)
          RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 16
            anchors.rightMargin: 12
            spacing: 12
            RecallIcon { name: "search"; ink: query.activeFocus ? Theme.Color.menu.selectedText : Qt.alpha(Theme.Color.menu.text, 0.5) }
            TextField {
              id: query
              objectName: "recallQuery"
              Layout.fillWidth: true
              Layout.fillHeight: true
              padding: 0
              color: Theme.Color.menu.text
              placeholderTextColor: Qt.alpha(Theme.Color.menu.text, 0.4)
              selectionColor: Theme.Color.menu.selectedBackground
              selectedTextColor: Theme.Color.menu.text
              background: Item {}
              placeholderText: "Find a file by what’s in it…"
              font.pixelSize: 16
              font.family: Theme.Style.fontFamily
              selectByMouse: true
              onTextChanged: { root.notice = ""; debounce.restart() }
              Keys.onEscapePressed: root.close()
              Keys.onDownPressed: { root.selected = Math.min(root.hits.length - 1, root.selected + 1); list.positionViewAtIndex(root.selected, ListView.Contain) }
              Keys.onUpPressed: { root.selected = Math.max(0, root.selected - 1); list.positionViewAtIndex(root.selected, ListView.Contain) }
              Keys.onReturnPressed: function(event) { root.activate((event.modifiers & Qt.ControlModifier) !== 0) }
              Keys.onEnterPressed: function(event) { root.activate((event.modifiers & Qt.ControlModifier) !== 0) }
            }
            RecallButton { visible: query.text.length > 0; iconName: "close"; compact: true; hint: "Clear search"; onClicked: { query.clear(); query.forceActiveFocus() } }
          }
          Rectangle {
            visible: root.searching
            anchors.bottom: parent.bottom
            height: 2
            width: parent.width * 0.25
            radius: 1
            color: Theme.Color.menu.selectedText
            SequentialAnimation on x {
              running: root.searching && root.opened && !root.showSettings
              loops: Animation.Infinite
              NumberAnimation { from: 0; to: panel.width - 240; duration: 900; easing.type: Easing.InOutSine }
              NumberAnimation { from: panel.width - 240; to: 0; duration: 900; easing.type: Easing.InOutSine }
            }
          }
        }
        RowLayout {
          visible: !root.showSettings
          Layout.fillWidth: true
          Layout.leftMargin: 24
          Layout.rightMargin: 24
          Layout.bottomMargin: 10
          Text { text: query.text.trim() ? (root.searching ? "SEARCHING" : root.hits.length + " RESULTS") : "YOUR FILES, WITH CONTEXT"; color: Qt.alpha(Theme.Color.menu.text, 0.45); font.pixelSize: 10; font.letterSpacing: 1.2; font.family: Theme.Style.fontFamily }
          Item { Layout.fillWidth: true }
          Text { text: "Local & private"; color: Qt.alpha(Theme.Color.menu.text, 0.45); font.pixelSize: 11; font.family: Theme.Style.fontFamily }
        }
        SettingsForm {
          id: settingsForm
          objectName: "recallSettings"
          visible: root.showSettings
          Layout.fillWidth: true
          Layout.fillHeight: true
          Layout.margins: 22
          onSaveRequested: function(settings) { root.settingsSave = root.send("configure", {config: settings}) }
          Keys.onEscapePressed: root.close()
        }
        ListView {
          id: list
          objectName: "recallResults"
          visible: !root.showSettings
          Layout.fillWidth: true
          Layout.fillHeight: true
          Layout.leftMargin: 12
          Layout.rightMargin: 12
          Layout.bottomMargin: 12
          clip: true
          spacing: 3
          model: root.hits
          ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
          delegate: Rectangle {
            id: result
            required property var modelData
            required property int index
            readonly property bool current: index === root.selected
            width: list.width
            height: 82
            radius: 7
            color: current ? Theme.Color.menu.selectedBackground : pointer.containsMouse ? Qt.alpha(Theme.Color.menu.text, 0.04) : "transparent"
            Behavior on color { ColorAnimation { duration: 90 } }
            Rectangle { visible: result.current; anchors.left: parent.left; anchors.verticalCenter: parent.verticalCenter; width: 2; height: 30; radius: 1; color: Theme.Color.menu.selectedText }
            RowLayout {
              anchors.fill: parent
              anchors.leftMargin: 14
              anchors.rightMargin: 14
              spacing: 14
              Rectangle {
                Layout.preferredWidth: 48
                Layout.preferredHeight: 48
                radius: 7
                color: Qt.alpha(Theme.Color.menu.text, 0.04)
                border.color: Qt.alpha(Theme.Color.menu.text, 0.06)
                RecallIcon { anchors.centerIn: parent; width: 23; height: 23; name: result.modelData.kind; ink: result.current ? Theme.Color.menu.selectedText : Qt.alpha(Theme.Color.menu.text, 0.55); visible: thumbnail.status !== Image.Ready }
                Image {
                  id: thumbnail
                  anchors.fill: parent
                  anchors.margins: 3
                  source: ["image", "screenshot"].indexOf(result.modelData.kind) >= 0 ? "file://" + encodeURI(result.modelData.path).replace(/#/g, "%23").replace(/\?/g, "%3F") : ""
                  sourceSize.width: 96
                  sourceSize.height: 96
                  fillMode: Image.PreserveAspectFit
                  asynchronous: true
                }
              }
              ColumnLayout {
                Layout.fillWidth: true
                spacing: 5
                RowLayout {
                  Layout.fillWidth: true
                  Text { text: result.modelData.name; color: Theme.Color.menu.text; font.weight: Font.Medium; font.pixelSize: 14; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; elide: Text.ElideRight; textFormat: Text.PlainText }
                  Text { text: result.modelData.location || result.modelData.kind; color: Qt.alpha(Theme.Color.menu.text, 0.5); font.pixelSize: 11; font.family: Theme.Style.fontFamily; Layout.maximumWidth: 160; elide: Text.ElideRight; textFormat: Text.PlainText }
                }
                Text { text: result.modelData.excerpt || result.modelData.kind; color: Qt.alpha(Theme.Color.menu.text, 0.7); font.pixelSize: 12; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; elide: Text.ElideRight; textFormat: Text.PlainText }
                Text { text: result.modelData.folder; color: Qt.alpha(Theme.Color.menu.text, 0.4); font.pixelSize: 11; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; elide: Text.ElideMiddle; textFormat: Text.PlainText }
              }
              RecallIcon { name: "arrow"; ink: Theme.Color.menu.selectedText; opacity: result.current ? 1 : 0; Layout.preferredWidth: 17; Layout.preferredHeight: 17 }
            }
            MouseArea { id: pointer; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: { root.selected = index; query.forceActiveFocus() }
              onDoubleClicked: { root.selected = index; root.activate(false) } }
          }
          ColumnLayout {
            anchors.centerIn: parent
            width: Math.min(420, parent.width - 32)
            visible: !root.hits.length
            spacing: 14
            Rectangle {
              Layout.alignment: Qt.AlignHCenter
              Layout.preferredWidth: 58
              Layout.preferredHeight: 58
              radius: 16
              color: Qt.alpha(Theme.Color.menu.selectedText, 0.07)
              RecallIcon { anchors.centerIn: parent; width: 26; height: 26; ink: Qt.alpha(Theme.Color.menu.selectedText, 0.8) }
            }
            Text { text: root.searching ? "Finding the right moment…" : query.text.trim() ? "No matches found" : "Remember the detail. Find the file."; color: Theme.Color.menu.text; font.pixelSize: 16; font.weight: Font.Medium; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; horizontalAlignment: Text.AlignHCenter; wrapMode: Text.Wrap }
            Text { text: root.searching ? "Searching your selected folders" : query.text.trim() ? "Try a different detail, phrase, or filename." : "Describe a document, a photo, or a moment in a recording."; color: Qt.alpha(Theme.Color.menu.text, 0.5); font.pixelSize: 12; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; horizontalAlignment: Text.AlignHCenter; wrapMode: Text.Wrap }
            RowLayout {
              visible: !query.text.trim() && list.width >= 400
              Layout.alignment: Qt.AlignHCenter
              Layout.topMargin: 8
              spacing: 8
              Repeater {
                model: ["a scanned receipt", "code: retry logic"]
                RecallButton { required property string modelData; text: modelData; onClicked: { query.text = modelData; query.forceActiveFocus() } }
              }
            }
          }
        }
        Rectangle { Layout.fillWidth: true; Layout.preferredHeight: 1; color: Qt.alpha(Theme.Color.menu.text, 0.1) }
        RowLayout {
          Layout.fillWidth: true
          Layout.preferredHeight: 35
          Layout.leftMargin: 22
          Layout.rightMargin: 22
          spacing: 8
          Rectangle { Layout.preferredWidth: 5; Layout.preferredHeight: 5; radius: 3; color: root.paused ? Qt.alpha(Theme.Color.menu.text, 0.4) : Theme.Color.menu.selectedText }
          Text { text: root.status; color: Qt.alpha(Theme.Color.menu.text, 0.5); font.pixelSize: 11; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; elide: Text.ElideRight; textFormat: Text.PlainText }
          Text { visible: !root.showSettings; text: panel.width >= 640 ? "↑ ↓  Select     ↵  Open     Ctrl ↵  Reveal     Esc  Close" : "↵  Open    Esc  Close"; color: Qt.alpha(Theme.Color.menu.text, 0.45); font.pixelSize: 11; font.family: Theme.Style.fontFamily }
        }
        RowLayout {
          visible: root.showSettings || root.notice.length > 0
          Layout.fillWidth: true
          Layout.leftMargin: 22
          Layout.rightMargin: 18
          Layout.bottomMargin: 12
          Text { text: root.notice; color: Theme.Color.menu.text; font.pixelSize: 11; font.family: Theme.Style.fontFamily; Layout.fillWidth: true; elide: Text.ElideRight; textFormat: Text.PlainText }
          RecallButton { visible: root.showSettings; text: "Save settings"; primary: true; onClicked: settingsForm.save() }
        }
      }
    }
  }
}
