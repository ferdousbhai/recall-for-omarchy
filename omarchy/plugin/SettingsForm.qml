import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons as Theme

ScrollView {
  id: form
  signal saveRequested(var settings)
  property var config: ({})
  function load(settings) {
    config = settings
    folders.text = (settings.roots || []).join("\n")
    codeFolders.text = (settings.code_roots || []).join("\n")
    excluded.text = (settings.excluded_folders || []).join("\n")
    codeExcluded.text = (settings.code_excluded || []).join("\n")
    types.text = (settings.excluded_types || []).join(", ")
    editor.currentIndex = Math.max(0, editor.model.indexOf(settings.code_editor || ""))
    battery.checked = settings.index_on_battery !== false
    idle.value = settings.encoder_idle_seconds || 120
  }
  function save() { form.saveRequested({roots: form.lines(folders.text), code_roots: form.lines(codeFolders.text), excluded_folders: form.lines(excluded.text), code_excluded: form.lines(codeExcluded.text), excluded_types: types.text.split(/[,\s]+/).filter(function(s) { return s.length > 0 }), code_editor: editor.currentText, index_on_battery: battery.checked, encoder_idle_seconds: idle.value}) }
  function lines(text) { return text.split(/\n/).map(function(s) { return s.trim() }).filter(function(s) { return s.length > 0 }) }
  clip: true
  ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
  component SectionTitle: Text {
    color: Theme.Color.menu.text
    font.pixelSize: 14
    font.weight: Font.DemiBold
    font.family: Theme.Style.fontFamily
    Layout.fillWidth: true
  }
  component Description: Text {
    color: Qt.alpha(Theme.Color.menu.text, 0.5)
    font.pixelSize: 11
    font.family: Theme.Style.fontFamily
    wrapMode: Text.Wrap
    Layout.fillWidth: true
  }
  component PathInput: TextArea {
    Layout.fillWidth: true
    implicitHeight: Math.max(68, contentHeight + 24)
    padding: 12
    color: Theme.Color.menu.text
    placeholderTextColor: Qt.alpha(Theme.Color.menu.text, 0.35)
    selectionColor: Theme.Color.menu.selectedBackground
    selectedTextColor: Theme.Color.menu.text
    font.pixelSize: 12
    font.family: Theme.Style.fontFamily
    selectByMouse: true
    wrapMode: TextEdit.Wrap
    background: Rectangle { radius: 6; color: Qt.alpha(Theme.Color.menu.text, 0.025); border.color: parent.activeFocus ? Qt.alpha(Theme.Color.menu.selectedText, 0.7) : Qt.alpha(Theme.Color.menu.text, 0.14) }
  }
  component Divider: Rectangle {
    Layout.fillWidth: true
    Layout.preferredHeight: 1
    Layout.topMargin: 8
    Layout.bottomMargin: 8
    color: Qt.alpha(Theme.Color.menu.text, 0.1)
  }
  ColumnLayout {
    width: form.availableWidth
    spacing: 10
    SectionTitle { text: "Your search library"; font.pixelSize: 19 }
    Description { text: "Choose what Recall can read. Your files and searches stay on this computer."; Layout.bottomMargin: 10 }
    SectionTitle { text: "Search folders" }
    Description { text: "Documents, pictures, audio and video. One absolute folder path per line." }
    PathInput { id: folders; placeholderText: "/home/you/Documents\n/home/you/Pictures" }
    SectionTitle { text: "Code repositories"; Layout.topMargin: 8 }
    Description { text: "Search these projects with code: followed by a name or an idea." }
    PathInput { id: codeFolders; placeholderText: "/home/you/projects" }
    Divider {}
    SectionTitle { text: "Keep out of search" }
    Description { text: "Excluded paths and file types are skipped inside your selected folders." }
    Description { text: "Excluded folders"; color: Qt.alpha(Theme.Color.menu.text, 0.8) }
    PathInput { id: excluded; placeholderText: "Absolute paths, one per line" }
    Description { text: "Excluded code folders"; color: Qt.alpha(Theme.Color.menu.text, 0.8) }
    PathInput { id: codeExcluded; placeholderText: "Absolute paths, one per line" }
    Description { text: "Excluded file extensions"; color: Qt.alpha(Theme.Color.menu.text, 0.8) }
    TextField {
      id: types
      Layout.fillWidth: true
      implicitHeight: 38
      padding: 12
      color: Theme.Color.menu.text
      placeholderTextColor: Qt.alpha(Theme.Color.menu.text, 0.35)
      placeholderText: "heic, csv"
      font.pixelSize: 12
      font.family: Theme.Style.fontFamily
      selectByMouse: true
      selectionColor: Theme.Color.menu.selectedBackground
      selectedTextColor: Theme.Color.menu.text
      background: Rectangle { radius: 6; color: Qt.alpha(Theme.Color.menu.text, 0.025); border.color: types.activeFocus ? Qt.alpha(Theme.Color.menu.selectedText, 0.7) : Qt.alpha(Theme.Color.menu.text, 0.14) }
    }
    Divider {}
    SectionTitle { text: "Preferences" }
    RowLayout {
      Layout.fillWidth: true
      Description { text: "Open code in"; color: Theme.Color.menu.text }
      ComboBox {
        id: editor
        Layout.preferredWidth: 210
        implicitHeight: 36
        model: ["", "nvim", "code", "codium", "cursor", "windsurf", "zed", "subl", "pycharm", "idea", "clion", "goland", "webstorm", "rustrover", "kate", "gnome-text-editor", "gedit"]
        displayText: currentText || "System default"
        font.pixelSize: 12
        font.family: Theme.Style.fontFamily
        palette.buttonText: Theme.Color.menu.text
        palette.text: Theme.Color.menu.text
        palette.button: Theme.Color.menu.background
        palette.base: Theme.Color.menu.background
        palette.window: Theme.Color.menu.background
        palette.highlight: Theme.Color.menu.selectedBackground
        palette.highlightedText: Theme.Color.menu.selectedText
        background: Rectangle { radius: 6; color: Qt.alpha(Theme.Color.menu.text, 0.025); border.color: editor.activeFocus ? Qt.alpha(Theme.Color.menu.selectedText, 0.7) : Qt.alpha(Theme.Color.menu.text, 0.14) }
      }
    }
    CheckBox {
      id: battery
      text: "Allow initial indexing on battery"
      font.pixelSize: 12
      font.family: Theme.Style.fontFamily
      palette.windowText: Theme.Color.menu.text
      palette.buttonText: Theme.Color.menu.text
      palette.highlight: Theme.Color.menu.selectedText
      palette.light: Theme.Color.menu.background
      palette.dark: Qt.alpha(Theme.Color.menu.text, 0.4)
    }
    RowLayout {
      Layout.fillWidth: true
      Description { text: "Release idle search model after"; color: Theme.Color.menu.text }
      SpinBox {
        id: idle
        from: 1
        to: 86400
        editable: true
        implicitHeight: 36
        font.pixelSize: 12
        font.family: Theme.Style.fontFamily
        palette.text: Theme.Color.menu.text
        palette.buttonText: Theme.Color.menu.text
        palette.button: Theme.Color.menu.background
        palette.base: Theme.Color.menu.background
        palette.highlight: Theme.Color.menu.selectedBackground
        background: Rectangle { radius: 6; color: Qt.alpha(Theme.Color.menu.text, 0.025); border.color: idle.activeFocus ? Qt.alpha(Theme.Color.menu.selectedText, 0.7) : Qt.alpha(Theme.Color.menu.text, 0.14) }
      }
      Description { text: "seconds"; Layout.fillWidth: false }
    }
    Divider {}
    SectionTitle { text: "Storage & text recognition" }
    Description { text: "Index  " + (form.config.index_directory || "") + "\nModels  " + (form.config.models_directory || ""); textFormat: Text.PlainText; wrapMode: Text.WrapAnywhere }
    Description { text: "Screenshot and scanned-page text uses installed Tesseract language packs. Run recall ocr to see available recognition languages."; Layout.topMargin: 4; Layout.bottomMargin: 12 }
  }
}
