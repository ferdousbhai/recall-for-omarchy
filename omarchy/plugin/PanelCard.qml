pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons
import qs.Commons as Commons

// Everything inside the panel: the search view, the settings view and the question asked before unsaved settings
// are dropped. It is its own file, apart from the layer-shell window in SearchPanel.qml, so the checks in
// tests/plugin can load and drive it without a compositor.
Rectangle {
  id: card
  required property Backend backend
  // Whether the panel is on screen; shortcuts only answer then.
  property bool active: false
  property bool showSettings: false
  property int selected: 0
  property alias queryText: query.text
  readonly property alias settings: settingsForm
  readonly property alias asking: confirm.opened
  // Emitted once the panel may really go away (never while the discard question is still open).
  signal closeRequested()

  // The panel has just been summoned, with the options the caller passed (`recall settings`, a preset query).
  function present(options: var) {
    if (options && options.query !== undefined) query.text = String(options.query).slice(0, 4096)
    // Settings left unsaved the last time are still in the form; show them rather than a search box hiding them.
    showSettings = (options && options.settings === true) || settingsForm.dirty
    if (!showSettings) backend.send("prewarm")
    else if (!settingsForm.dirty) backend.send("config")
    backend.send("status")
    Qt.callLater(card.focusView)
  }
  function focusView() {
    if (confirm.opened) confirm.forceActiveFocus()
    else if (card.showSettings) settingsForm.focusFirst()
    else query.forceActiveFocus()
  }
  // Leave the settings view, for the search view ("search") or by closing the panel ("close"). Unsaved settings
  // are asked about first, whichever way the request came: Esc, the buttons, a click outside or the hotkey.
  function requestLeave(target: string) {
    if (confirm.opened) return
    if (card.showSettings && settingsForm.dirty) {
      confirm.pending = target
      // Enter on a freshly opened question must not lose anything, so "Keep editing" starts selected.
      confirm.selectedIndex = 0
      confirm.opened = true
      confirm.forceActiveFocus()
      return
    }
    card.leave(target)
  }
  function leave(target: string) {
    if (target === "close") {
      debounce.stop()
      card.closeRequested()
    } else {
      card.showSettings = false
      query.forceActiveFocus()
    }
  }
  function requestClose() { card.requestLeave("close") }
  function toggleSettings() {
    if (card.showSettings) {
      card.requestLeave("search")
    } else {
      card.showSettings = true
      card.backend.send("config")
      settingsForm.focusFirst()
    }
  }
  function togglePause() { card.backend.setPaused(!card.backend.paused) }
  function move(step: int) {
    card.selected = Math.max(0, Math.min(card.backend.hits.length - 1, card.selected + step))
    list.positionViewAtIndex(card.selected, ListView.Contain)
  }
  function activate(reveal: bool) {
    var hits = card.backend.hits
    if (hits.length && card.selected >= 0 && card.selected < hits.length) {
      card.backend.send(reveal ? "reveal" : "open", {hit: hits[card.selected]})
      card.requestClose()
    }
  }
  function number(value: var): string { return Commons.I18n.formatNumber(Number(value) || 0) }
  function kindLabel(kind: string): string {
    switch (kind) {
    case "image": return strings.t("kind.image")
    case "screenshot": return strings.t("kind.screenshot")
    case "pdf": return strings.t("kind.pdf")
    case "doc": return strings.t("kind.doc")
    case "audio": return strings.t("kind.audio")
    case "video": return strings.t("kind.video")
    case "code": return strings.t("kind.code")
    }
    return kind
  }
  // Where in the file the match is. Built here from the hit's numbers rather than taken from the daemon's English
  // label, so the words and the digits follow the language.
  function locationLabel(hit: var): string {
    var start = Math.floor(Number(hit.loc) || 0)
    var end = Math.floor(Number(hit.loc_end) || start)
    switch (hit.segment) {
    case "page":
    case "page_image": return strings.t("hit.page").arg(card.number(start))
    case "audio":
    case "frame": {
      let seconds = card.number(start % 60)
      if (start % 60 < 10) seconds = card.number(0) + seconds
      return strings.t("hit.time").arg(card.number(Math.floor(start / 60))).arg(seconds)
    }
    case "lines": return strings.t("hit.lines").arg(card.number(start)).arg(card.number(end))
    case "chunk": return strings.t("hit.part").arg(card.number(start))
    case "cells": return strings.t("hit.cell").arg(card.number(start))
    }
    return card.kindLabel(String(hit.kind || ""))
  }

  // What is wrong with the connection, in words the user can act on; empty while it works.
  readonly property string problemTitle: {
    switch (card.backend.link) {
    case "missing": return strings.t("problem.missing.title")
    case "unavailable": return strings.t("problem.unavailable.title")
    case "stopped": return strings.t("problem.stopped.title")
    }
    return ""
  }
  readonly property string problemDetail: {
    switch (card.backend.link) {
    case "missing": return strings.t("problem.missing.detail").arg(card.backend.clientPath)
    case "unavailable": return strings.t("problem.unavailable.detail")
    case "stopped": return strings.t("problem.stopped.detail")
    }
    return ""
  }
  readonly property string statusText: {
    if (card.problemTitle.length > 0) return card.problemTitle
    var s = card.backend.info
    if (!s) return strings.t("status.starting")
    if (!(s.roots && s.roots.length) && !(s.code_roots && s.code_roots.length)) return strings.t("status.chooseFolders")
    if (s.download === "downloading" && s.download_total > 0)
      return strings.t("status.downloading").arg(card.number(Math.floor(s.download_bytes / s.download_total * 100)))
    if (s.download === "retrying") return strings.t("status.downloadRetrying").arg(String(s.line || "").replace(/^Model download: /, ""))
    if (card.backend.paused) return strings.t("status.paused")
    switch (s.activity) {
    case "indexing": return strings.t("status.indexing").arg(card.number(s.done)).arg(card.number(s.total))
    case "syncing": return strings.t("status.checking")
    case "waiting": return strings.t("status.waiting")
    case "error": return strings.t("status.error").arg(String(s.message || ""))
    case undefined: return strings.t("status.starting")
    }
    return strings.tn("status.indexed", Number(s.searchable) || 0).arg(card.number(s.searchable))
  }
  readonly property string noticeText: {
    switch (card.backend.noticeKind) {
    case "saved": return strings.t("notice.saved")
    case "unreadable": return strings.t("notice.unreadable")
    case "semantic": return strings.t("notice.semantic")
    case "warming": return strings.t("notice.warming")
    case "exactOnly": return strings.t("notice.exactOnly").arg(card.backend.noticeDetail)
    case "error": return card.backend.noticeDetail
    }
    return ""
  }
  readonly property bool hasQuery: query.text.trim().length > 0
  // Every shortcut the panel answers, shown in the footer so none has to be remembered. Keys stay as printed on
  // the keyboard; only what they do is translated.
  readonly property var hints: card.showSettings ? [
    {keys: "Tab", label: strings.t("hint.nextField")},
    {keys: "Ctrl S", label: strings.t("hint.save")},
    {keys: "Ctrl P", label: card.backend.paused ? strings.t("hint.resume") : strings.t("hint.pause")},
    {keys: "Ctrl ,", label: strings.t("hint.search")},
    {keys: "Esc", label: strings.t("hint.close")}
  ] : [
    {keys: "↑ ↓", label: strings.t("hint.select")},
    {keys: "↵", label: strings.t("hint.open")},
    {keys: "Ctrl ↵", label: strings.t("hint.reveal")},
    {keys: "Ctrl P", label: card.backend.paused ? strings.t("hint.resume") : strings.t("hint.pause")},
    {keys: "Ctrl ,", label: strings.t("hint.settings")},
    {keys: "Esc", label: strings.t("hint.close")}
  ]

  color: theme.background
  border.color: theme.dim(0.22)
  border.width: Style.spacing.hairline
  // Corners follow Hyprland's window rounding, which is sharp on a stock Omarchy.
  radius: Style.cornerRadius
  // Arabic, Hebrew, Persian and Urdu read the whole panel mirrored.
  LayoutMirroring.enabled: Commons.I18n.rightToLeft
  LayoutMirroring.childrenInherit: true
  // Keys no field wanted end up here; Esc always means "leave", from any field.
  Keys.onEscapePressed: card.requestClose()

  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }
  RecallTheme { id: theme }
  Timer { id: debounce; interval: 250; onTriggered: card.backend.search(query.text) }
  Connections {
    target: card.backend
    function onConfigReceived(config, saved) {
      // A reply that arrives after typing has begun must not wipe it; only a completed save replaces the form.
      if (saved || !settingsForm.dirty) settingsForm.load(config)
      if (saved) card.leave("search")
    }
    function onHitsChanged() { card.selected = 0 }
  }

  // The panel is typed into, so its actions sit on Ctrl and a key rather than on bare letters.
  Shortcut { sequences: ["Ctrl+P"]; enabled: card.active && !confirm.opened; onActivated: card.togglePause() }
  Shortcut { sequences: ["Ctrl+,"]; enabled: card.active && !confirm.opened; onActivated: card.toggleSettings() }
  Shortcut { sequences: ["Ctrl+S"]; enabled: card.active && card.showSettings && !confirm.opened; onActivated: settingsForm.save() }

  component Label: Text {
    textFormat: Text.PlainText
    // Stated outright so it mirrors: left-to-right content such as a file name then still starts at the reading
    // edge in a right-to-left language instead of drifting to the far side of its row.
    horizontalAlignment: Text.AlignLeft
    color: theme.text
    font.pixelSize: Style.font.body
    font.family: Style.font.family
  }
  component Hairline: Rectangle {
    Layout.fillWidth: true
    Layout.preferredHeight: Style.spacing.hairline
    color: theme.dim(0.1)
  }

  // Under everything else: a click on an empty part of the card stops here instead of reaching the scrim behind
  // it, which would close the panel.
  MouseArea { anchors.fill: parent }
  ColumnLayout {
    anchors.fill: parent
    anchors.margins: card.border.width
    spacing: 0
    RowLayout {
      Layout.fillWidth: true
      Layout.preferredHeight: Style.space(60)
      Layout.leftMargin: Style.space(22)
      Layout.rightMargin: Style.space(14)
      spacing: Style.space(10)
      RecallIcon { name: "search"; ink: theme.accent; Layout.preferredWidth: Style.space(22); Layout.preferredHeight: Style.space(22) }
      Label { text: "Recall"; font.pixelSize: Style.font.heading; font.weight: Font.DemiBold }
      Label { visible: card.width >= Style.space(500); text: card.showSettings ? strings.t("header.settings") : strings.t("header.search"); color: theme.dim(0.45); Layout.fillWidth: true; elide: Text.ElideRight }
      Item { Layout.fillWidth: true; visible: card.width < Style.space(500) }
      RecallButton { objectName: "recallPause"; iconName: card.backend.paused ? "play" : "pause"; compact: true; hint: (card.backend.paused ? strings.t("action.resume") : strings.t("action.pause")).arg("Ctrl+P"); onClicked: card.togglePause() }
      RecallButton { objectName: "recallSettingsToggle"; iconName: card.showSettings ? "search" : "settings"; compact: true; hint: (card.showSettings ? strings.t("action.backToSearch") : strings.t("action.settings")).arg("Ctrl+,"); onClicked: card.toggleSettings() }
      Rectangle { Layout.preferredWidth: Style.spacing.hairline; Layout.preferredHeight: Style.space(18); color: theme.dim(0.12) }
      RecallButton { iconName: "close"; compact: true; hint: strings.t("action.close").arg("Esc"); onClicked: card.requestClose() }
    }
    Hairline {}
    Rectangle {
      visible: !card.showSettings
      Layout.fillWidth: true
      Layout.preferredHeight: Style.space(54)
      Layout.margins: Style.space(18)
      radius: Style.cornerRadius
      color: theme.dim(0.025)
      border.width: Style.spacing.hairline
      border.color: query.activeFocus ? theme.tint(0.7) : theme.dim(0.2)
      RowLayout {
        anchors.fill: parent
        anchors.leftMargin: Style.space(16)
        anchors.rightMargin: Style.space(12)
        spacing: Style.space(12)
        RecallIcon { name: "search"; ink: query.activeFocus ? theme.accent : theme.dim(0.5) }
        TextField {
          id: query
          objectName: "recallQuery"
          Layout.fillWidth: true
          Layout.fillHeight: true
          padding: 0
          color: theme.text
          placeholderTextColor: theme.dim(0.4)
          selectionColor: theme.selection
          selectedTextColor: theme.text
          background: Item {}
          placeholderText: strings.t("search.placeholder")
          font.pixelSize: Style.font.heading
          font.family: Style.font.family
          selectByMouse: true
          horizontalAlignment: TextInput.AlignLeft
          onTextChanged: { card.backend.setNotice("", ""); debounce.restart() }
          Keys.onDownPressed: card.move(1)
          Keys.onUpPressed: card.move(-1)
          Keys.onReturnPressed: function(event) { card.activate((event.modifiers & Qt.ControlModifier) !== 0) }
          Keys.onEnterPressed: function(event) { card.activate((event.modifiers & Qt.ControlModifier) !== 0) }
        }
        RecallButton { visible: query.text.length > 0; iconName: "close"; compact: true; hint: strings.t("action.clear"); onClicked: { query.clear(); query.forceActiveFocus() } }
      }
      // An indeterminate bar while a search runs. It travels at a constant speed, like a spinner, and stands
      // still as a full-width line when animations are turned off (a zero-length loop would otherwise spin).
      Rectangle {
        id: busy
        readonly property real travel: Math.max(0, parent.width - width)
        visible: card.backend.searching
        anchors.bottom: parent.bottom
        height: Math.max(1, Style.space(2))
        width: Style.reduceMotion ? parent.width : parent.width * 0.25
        color: theme.accent
        SequentialAnimation on x {
          running: card.backend.searching && card.active && !card.showSettings && !Style.reduceMotion
          loops: Animation.Infinite
          NumberAnimation { from: 0; to: busy.travel; duration: Style.duration(900) }
          NumberAnimation { from: busy.travel; to: 0; duration: Style.duration(900) }
        }
      }
    }
    RowLayout {
      visible: !card.showSettings
      Layout.fillWidth: true
      Layout.leftMargin: Style.space(24)
      Layout.rightMargin: Style.space(24)
      Layout.bottomMargin: Style.space(10)
      spacing: Style.space(12)
      Label {
        objectName: "recallCount"
        // Counted through the plural form, so "1 result" and languages with several plurals read correctly.
        text: !card.hasQuery ? strings.t("search.tagline")
          : card.backend.searching ? strings.t("search.searching")
          : strings.tn("search.results", card.backend.hits.length).arg(card.number(card.backend.hits.length))
        color: theme.dim(0.45)
        font.pixelSize: Style.font.caption
        font.letterSpacing: 1.2
        font.capitalization: Font.AllUppercase
        Layout.fillWidth: true
        elide: Text.ElideRight
      }
      Label { text: strings.t("search.private"); color: theme.dim(0.45); font.pixelSize: Style.font.bodySmall }
    }
    SettingsForm {
      id: settingsForm
      objectName: "recallSettings"
      visible: card.showSettings
      Layout.fillWidth: true
      Layout.fillHeight: true
      Layout.margins: Style.space(22)
      onSaveRequested: function(settings) { card.backend.save(settings) }
    }
    ListView {
      id: list
      objectName: "recallResults"
      visible: !card.showSettings
      Layout.fillWidth: true
      Layout.fillHeight: true
      Layout.leftMargin: Style.space(12)
      Layout.rightMargin: Style.space(12)
      Layout.bottomMargin: Style.space(12)
      clip: true
      spacing: Style.space(3)
      model: card.backend.hits
      ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
      delegate: Rectangle {
        id: result
        required property var modelData
        required property int index
        readonly property bool current: index === card.selected
        width: list.width
        height: Style.space(82)
        radius: Style.cornerRadius
        color: current ? theme.selection : pointer.containsMouse ? theme.dim(0.04) : "transparent"
        Behavior on color { ColorAnimation { duration: Style.duration(90) } }
        Rectangle { visible: result.current; anchors.left: parent.left; anchors.verticalCenter: parent.verticalCenter; width: Math.max(1, Style.space(2)); height: Style.space(30); color: theme.accent }
        RowLayout {
          anchors.fill: parent
          anchors.leftMargin: Style.space(14)
          anchors.rightMargin: Style.space(14)
          spacing: Style.space(14)
          Rectangle {
            Layout.preferredWidth: Style.space(48)
            Layout.preferredHeight: Style.space(48)
            radius: Style.cornerRadius
            color: theme.dim(0.04)
            border.color: theme.dim(0.06)
            RecallIcon { anchors.centerIn: parent; width: Style.space(23); height: Style.space(23); name: result.modelData.kind; ink: result.current ? theme.accent : theme.dim(0.55); visible: thumbnail.status !== Image.Ready }
            Image {
              id: thumbnail
              anchors.fill: parent
              anchors.margins: Style.space(3)
              // Only pictures get a thumbnail, and "#" and "?" in a file name must not be read as URL syntax.
              source: ["image", "screenshot"].indexOf(result.modelData.kind) >= 0 ? "file://" + encodeURI(result.modelData.path).replace(/#/g, "%23").replace(/\?/g, "%3F") : ""
              sourceSize.width: 96
              sourceSize.height: 96
              fillMode: Image.PreserveAspectFit
              asynchronous: true
            }
          }
          ColumnLayout {
            Layout.fillWidth: true
            spacing: Style.space(5)
            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(10)
              Label { text: result.modelData.name; font.weight: Font.Medium; font.pixelSize: Style.font.title; Layout.fillWidth: true; elide: Text.ElideRight }
              Label { text: card.locationLabel(result.modelData); color: theme.dim(0.5); font.pixelSize: Style.font.bodySmall; Layout.maximumWidth: Style.space(160); elide: Text.ElideRight }
            }
            Label { text: result.modelData.excerpt || card.kindLabel(String(result.modelData.kind || "")); color: theme.dim(0.7); Layout.fillWidth: true; elide: Text.ElideRight }
            // Paths read left to right in every language, and the middle is the part that can be spared.
            Label { text: result.modelData.folder; color: theme.dim(0.4); font.pixelSize: Style.font.bodySmall; Layout.fillWidth: true; elide: Text.ElideMiddle }
          }
          RecallIcon { name: "arrow"; directional: true; ink: theme.accent; opacity: result.current ? 1 : 0; Layout.preferredWidth: Style.space(17); Layout.preferredHeight: Style.space(17) }
        }
        MouseArea {
          id: pointer
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          // A click only selects, and hands the keyboard back to the search box so typing and Enter keep working.
          onClicked: { card.selected = result.index; query.forceActiveFocus() }
          onDoubleClicked: { card.selected = result.index; card.activate(false) }
        }
      }
      ColumnLayout {
        anchors.centerIn: parent
        width: Math.min(Style.space(460), parent.width - Style.space(32))
        visible: card.backend.hits.length === 0
        spacing: Style.space(14)
        Rectangle {
          Layout.alignment: Qt.AlignHCenter
          Layout.preferredWidth: Style.space(58)
          Layout.preferredHeight: Style.space(58)
          radius: Style.cornerRadius
          color: theme.tint(0.07)
          RecallIcon { anchors.centerIn: parent; width: Style.space(26); height: Style.space(26); ink: theme.tint(0.8) }
        }
        Label {
          objectName: "recallEmptyTitle"
          // A broken connection is said here, where the results would be, with what to do about it underneath.
          text: card.problemTitle.length > 0 ? card.problemTitle
            : card.backend.searching ? strings.t("empty.searching")
            : card.hasQuery ? strings.t("empty.none") : strings.t("empty.intro")
          font.pixelSize: Style.font.heading
          font.weight: Font.Medium
          Layout.fillWidth: true
          horizontalAlignment: Text.AlignHCenter
          wrapMode: Text.Wrap
        }
        Label {
          objectName: "recallEmptyDetail"
          visible: text.length > 0
          text: card.problemDetail.length > 0 ? card.problemDetail
            : card.backend.searching ? ""
            : card.hasQuery ? strings.t("empty.noneHelp") : strings.t("empty.introHelp")
          color: theme.dim(0.5)
          Layout.fillWidth: true
          horizontalAlignment: Text.AlignHCenter
          wrapMode: Text.Wrap
        }
        Flow {
          visible: !card.hasQuery && card.problemTitle.length === 0 && list.width >= Style.space(400)
          Layout.alignment: Qt.AlignHCenter
          Layout.maximumWidth: parent.width
          Layout.topMargin: Style.space(8)
          spacing: Style.space(8)
          RecallButton { id: receiptExample; text: strings.t("example.receipt"); onClicked: { query.text = receiptExample.text; query.forceActiveFocus() } }
          // "code:" is the prefix the engine looks for, so it stays as it is and only the words after it change.
          RecallButton { id: codeExample; text: "code: " + strings.t("example.code"); onClicked: { query.text = codeExample.text; query.forceActiveFocus() } }
        }
      }
    }
    Hairline {}
    RowLayout {
      Layout.fillWidth: true
      Layout.minimumHeight: Style.space(35)
      Layout.leftMargin: Style.space(22)
      Layout.rightMargin: Style.space(22)
      Layout.topMargin: Style.space(6)
      Layout.bottomMargin: Style.space(6)
      spacing: Style.space(8)
      // A true circle, so it keeps its radius whatever the window rounding is.
      Rectangle { Layout.preferredWidth: Style.space(5); Layout.preferredHeight: Style.space(5); radius: width / 2; color: card.backend.paused || card.problemTitle.length > 0 ? theme.dim(0.4) : theme.accent }
      Label { objectName: "recallStatus"; text: card.statusText; color: theme.dim(0.5); font.pixelSize: Style.font.bodySmall; Layout.fillWidth: true; Layout.minimumWidth: Style.space(120); elide: Text.ElideRight }
      // Wraps onto further lines when a language's words are long, rather than being cut off.
      Flow {
        objectName: "recallHints"
        Layout.maximumWidth: card.width * 0.72
        Layout.alignment: Qt.AlignVCenter
        spacing: Style.space(14)
        Repeater {
          // The narrowest panels keep only what opens a result and what closes the panel.
          model: card.width >= Style.space(640) ? card.hints : card.hints.filter(function(hint) { return hint.keys === "↵" || hint.keys === "Esc" || hint.keys === "Ctrl S" })
          Row {
            id: chip
            required property var modelData
            spacing: Style.space(6)
            Label { text: chip.modelData.keys; color: theme.dim(0.6); font.pixelSize: Style.font.bodySmall }
            Label { text: chip.modelData.label; color: theme.dim(0.45); font.pixelSize: Style.font.bodySmall }
          }
        }
      }
    }
    RowLayout {
      visible: card.showSettings || card.noticeText.length > 0
      Layout.fillWidth: true
      Layout.leftMargin: Style.space(22)
      Layout.rightMargin: Style.space(18)
      Layout.bottomMargin: Style.space(12)
      spacing: Style.space(12)
      Label {
        objectName: "recallNotice"
        text: card.showSettings && settingsForm.dirty && card.backend.noticeKind !== "error" ? strings.t("notice.unsaved") : card.noticeText
        font.pixelSize: Style.font.bodySmall
        Layout.fillWidth: true
        elide: Text.ElideRight
      }
      RecallButton { objectName: "recallSave"; visible: card.showSettings; text: strings.t("action.save"); hint: strings.t("action.saveHint").arg("Ctrl+S"); primary: true; onClicked: settingsForm.save() }
    }
  }

  // Asked before unsaved settings are dropped. Dismissing it (Esc, a click beside it) keeps the edits. It is drawn
  // here rather than with the shell's ConfirmDialog because that one's buttons have a fixed width, which longer
  // captions in other languages overflow.
  Rectangle {
    id: confirm
    property bool opened: false
    // Where the user was heading when asked: "close" or "search".
    property string pending: "close"
    // 0 is "Keep editing", 1 is "Discard".
    property int selectedIndex: 0
    function keep() {
      confirm.opened = false
      settingsForm.focusFirst()
    }
    function discard() {
      confirm.opened = false
      settingsForm.revert()
      card.leave(confirm.pending)
    }
    anchors.fill: parent
    visible: opened
    radius: card.radius
    color: Qt.alpha(theme.background, 0.7)
    Keys.onPressed: function(event) {
      if (event.key === Qt.Key_Escape) confirm.keep()
      else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) confirm.selectedIndex === 1 ? confirm.discard() : confirm.keep()
      else if ([Qt.Key_Left, Qt.Key_Right, Qt.Key_Tab, Qt.Key_Backtab].indexOf(event.key) >= 0) confirm.selectedIndex = 1 - confirm.selectedIndex
      // Every key stops here while the question is open, so none reaches the form underneath.
      event.accepted = true
    }
    MouseArea { anchors.fill: parent; hoverEnabled: true; onClicked: confirm.keep() }
    Rectangle {
      anchors.centerIn: parent
      width: Math.min(parent.width - Style.space(32), Math.max(Style.space(370), answers.implicitWidth + Style.space(36)))
      height: question.implicitHeight + Style.space(36)
      radius: Style.cornerRadius
      color: theme.background
      border.width: Style.spacing.hairline
      border.color: theme.accent
      // A click on the question itself is not a click beside it.
      MouseArea { anchors.fill: parent }
      ColumnLayout {
        id: question
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: Style.space(18)
        spacing: Style.space(18)
        Label { text: strings.t("discard.message"); font.pixelSize: Style.font.title; wrapMode: Text.Wrap; Layout.fillWidth: true }
        RowLayout {
          id: answers
          Layout.alignment: Qt.AlignRight
          spacing: Style.space(10)
          // The buttons stay out of the focus chain: the question holds the keyboard and marks the chosen answer.
          RecallButton { objectName: "recallKeep"; text: strings.t("discard.keep"); primary: confirm.selectedIndex === 0; focusPolicy: Qt.NoFocus; onClicked: confirm.keep() }
          RecallButton { objectName: "recallDiscard"; text: strings.t("discard.confirm"); danger: true; primary: confirm.selectedIndex === 1; focusPolicy: Qt.NoFocus; onClicked: confirm.discard() }
        }
      }
    }
  }
}
