pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons
import qs.Commons as Commons

ScrollView {
  id: form
  signal saveRequested(var settings)
  // The settings as the daemon last reported them; what the fields are compared against.
  property var config: ({})
  property bool loaded: false
  readonly property var editors: ["", "nvim", "code", "codium", "cursor", "windsurf", "zed", "subl", "pycharm", "idea", "clion", "goland", "webstorm", "rustrover", "kate", "gnome-text-editor", "gedit"]

  // What the fields say now, in the shape the daemon stores. A binding, so `dirty` follows every keystroke.
  readonly property var edited: ({
    roots: form.lines(folders.text),
    code_roots: form.lines(codeFolders.text),
    excluded_folders: form.lines(excluded.text),
    code_excluded: form.lines(codeExcluded.text),
    excluded_types: form.extensions(types.text),
    code_editor: editor.currentIndex >= 0 ? form.editors[editor.currentIndex] : "",
    index_on_battery: battery.checked,
    encoder_idle_seconds: idle.value
  })
  // Unsaved means the content differs from what was saved, not that something was typed: editing a field back to
  // its saved value is clean again, and so is whitespace the daemon would never see.
  readonly property bool dirty: loaded && JSON.stringify(edited) !== JSON.stringify(form.stored(config))

  function stored(settings: var): var {
    var s = settings || {}
    return {
      roots: s.roots || [],
      code_roots: s.code_roots || [],
      excluded_folders: s.excluded_folders || [],
      code_excluded: s.code_excluded || [],
      excluded_types: s.excluded_types || [],
      code_editor: form.editors.indexOf(s.code_editor || "") >= 0 ? (s.code_editor || "") : "",
      index_on_battery: s.index_on_battery !== false,
      encoder_idle_seconds: s.encoder_idle_seconds || 120
    }
  }
  function load(settings: var) {
    config = settings || {}
    revert()
    loaded = true
  }
  // Put the fields back to the saved settings.
  function revert() {
    var s = stored(config)
    folders.text = s.roots.join("\n")
    codeFolders.text = s.code_roots.join("\n")
    excluded.text = s.excluded_folders.join("\n")
    codeExcluded.text = s.code_excluded.join("\n")
    types.text = s.excluded_types.join(", ")
    editor.currentIndex = form.editors.indexOf(s.code_editor)
    battery.checked = s.index_on_battery
    idle.value = s.encoder_idle_seconds
  }
  function save() {
    // A number still being typed only becomes the value when the box loses focus; take it now so Ctrl+S saves
    // what is on screen.
    var typed = idle.contentItem as TextInput
    if (typed && typed.activeFocus) idle.value = idle.valueFromText(typed.text, idle.locale)
    form.saveRequested(form.edited)
  }
  function lines(text: string): var { return text.split(/\n/).map(function(s) { return s.trim() }).filter(function(s) { return s.length > 0 }) }
  function extensions(text: string): var { return text.split(/[,\s]+/).filter(function(s) { return s.length > 0 }) }
  function focusFirst() { folders.forceActiveFocus(Qt.TabFocusReason) }
  // Keep the field the keyboard has just moved to on screen; Tab would otherwise walk off the visible part.
  function reveal(item: Item) {
    var flick = form.contentItem as Flickable
    if (!flick) return
    var top = item.mapToItem(column, 0, 0).y
    var margin = Style.space(12)
    if (top - margin < flick.contentY) flick.contentY = Math.max(0, top - margin)
    else if (top + item.height + margin > flick.contentY + flick.height)
      flick.contentY = Math.min(Math.max(0, flick.contentHeight - flick.height), top + item.height + margin - flick.height)
  }

  clip: true
  ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }
  RecallTheme { id: theme }

  component SectionTitle: Text {
    textFormat: Text.PlainText
    // Stated outright so that right-to-left languages mirror it, whatever script the text itself is in.
    horizontalAlignment: Text.AlignLeft
    color: theme.text
    font.pixelSize: Style.font.title
    font.weight: Font.DemiBold
    font.family: Style.font.family
    wrapMode: Text.Wrap
    Layout.fillWidth: true
  }
  component Description: Text {
    textFormat: Text.PlainText
    horizontalAlignment: Text.AlignLeft
    color: theme.dim(0.5)
    font.pixelSize: Style.font.bodySmall
    font.family: Style.font.family
    wrapMode: Text.Wrap
    Layout.fillWidth: true
  }
  component FieldLabel: Description {
    color: theme.dim(0.8)
  }
  component FieldFrame: Rectangle {
    property bool focused: false
    radius: Style.cornerRadius
    color: theme.dim(0.025)
    border.width: Style.spacing.hairline
    border.color: focused ? theme.tint(0.7) : theme.dim(0.14)
  }
  component PathInput: TextArea {
    id: pathInput
    Layout.fillWidth: true
    implicitHeight: Math.max(Style.space(68), contentHeight + Style.space(24))
    padding: Style.space(12)
    color: theme.text
    placeholderTextColor: theme.dim(0.35)
    selectionColor: theme.selection
    selectedTextColor: theme.text
    font.pixelSize: Style.font.body
    font.family: Style.font.family
    textFormat: TextEdit.PlainText
    horizontalAlignment: TextEdit.AlignLeft
    selectByMouse: true
    wrapMode: TextEdit.Wrap
    // A text area takes Tab as a character to insert, which would trap the keyboard in the first field. Paths
    // never contain tabs, so Tab moves on like it does in every other field.
    Keys.onTabPressed: pathInput.nextItemInFocusChain(true).forceActiveFocus(Qt.TabFocusReason)
    Keys.onBacktabPressed: pathInput.nextItemInFocusChain(false).forceActiveFocus(Qt.BacktabFocusReason)
    onActiveFocusChanged: if (activeFocus) form.reveal(pathInput)
    background: FieldFrame { focused: pathInput.activeFocus }
  }
  component Divider: Rectangle {
    Layout.fillWidth: true
    Layout.preferredHeight: Style.spacing.hairline
    Layout.topMargin: Style.space(8)
    Layout.bottomMargin: Style.space(8)
    color: theme.dim(0.1)
  }

  ColumnLayout {
    id: column
    width: form.availableWidth
    spacing: Style.space(10)
    SectionTitle { text: strings.t("settings.title"); font.pixelSize: Style.font.heading }
    Description { text: strings.t("settings.intro"); Layout.bottomMargin: Style.space(10) }
    SectionTitle { text: strings.t("settings.folders") }
    Description { text: strings.t("settings.foldersHelp") }
    PathInput { id: folders; placeholderText: "/home/you/Documents\n/home/you/Pictures" }
    SectionTitle { text: strings.t("settings.code"); Layout.topMargin: Style.space(8) }
    Description { text: strings.t("settings.codeHelp") }
    PathInput { id: codeFolders; placeholderText: "/home/you/projects" }
    Divider {}
    SectionTitle { text: strings.t("settings.exclusions") }
    FieldLabel { text: strings.t("settings.excludedFolders") }
    PathInput { id: excluded; placeholderText: strings.t("settings.pathsPlaceholder") }
    FieldLabel { text: strings.t("settings.excludedCode") }
    PathInput { id: codeExcluded; placeholderText: strings.t("settings.pathsPlaceholder") }
    FieldLabel { text: strings.t("settings.excludedTypes") }
    TextField {
      id: types
      Layout.fillWidth: true
      implicitHeight: Style.space(38)
      padding: Style.space(12)
      color: theme.text
      placeholderTextColor: theme.dim(0.35)
      placeholderText: "heic, csv"
      horizontalAlignment: TextInput.AlignLeft
      font.pixelSize: Style.font.body
      font.family: Style.font.family
      selectByMouse: true
      selectionColor: theme.selection
      selectedTextColor: theme.text
      onActiveFocusChanged: if (activeFocus) form.reveal(types)
      background: FieldFrame { focused: types.activeFocus }
    }
    Divider {}
    SectionTitle { text: strings.t("settings.preferences") }
    RowLayout {
      Layout.fillWidth: true
      spacing: Style.space(10)
      Description { text: strings.t("settings.editor"); color: theme.text }
      ComboBox {
        id: editor
        Layout.preferredWidth: Style.space(210)
        implicitHeight: Style.space(36)
        model: form.editors
        // The commands are names to run, so only the empty choice has words to translate.
        displayText: currentText || strings.t("settings.editorDefault")
        font.pixelSize: Style.font.body
        font.family: Style.font.family
        palette.buttonText: theme.text
        palette.text: theme.text
        palette.button: theme.background
        palette.base: theme.background
        palette.window: theme.background
        palette.highlight: theme.selection
        palette.highlightedText: theme.accent
        onActiveFocusChanged: if (activeFocus) form.reveal(editor)
        background: FieldFrame { focused: editor.activeFocus }
      }
    }
    CheckBox {
      id: battery
      Layout.fillWidth: true
      text: strings.t("settings.battery")
      font.pixelSize: Style.font.body
      font.family: Style.font.family
      palette.windowText: theme.text
      palette.buttonText: theme.text
      palette.highlight: theme.accent
      palette.light: theme.background
      palette.dark: theme.dim(0.4)
      onActiveFocusChanged: if (activeFocus) form.reveal(battery)
    }
    RowLayout {
      Layout.fillWidth: true
      spacing: Style.space(10)
      // One whole sentence beside the number box rather than words on either side of it, since other languages
      // order them differently.
      Description { text: strings.t("settings.idle"); color: theme.text }
      SpinBox {
        id: idle
        from: 1
        to: 86400
        editable: true
        implicitHeight: Style.space(36)
        // Room for five digits (a day in seconds) beside the arrows at any text size.
        Layout.preferredWidth: Style.space(120)
        // Digits and grouping follow the Omarchy language, not the locale the shell happened to start with.
        locale: Qt.locale(Commons.I18n.formatLocale)
        font.pixelSize: Style.font.body
        font.family: Style.font.family
        palette.text: theme.text
        palette.buttonText: theme.text
        palette.button: theme.background
        palette.base: theme.background
        palette.highlight: theme.selection
        onActiveFocusChanged: if (activeFocus) form.reveal(idle)
        background: FieldFrame { focused: idle.activeFocus }
      }
    }
    Divider {}
    SectionTitle { text: strings.t("settings.storage") }
    Description { text: strings.t("settings.indexPath").arg(form.config.index_directory || ""); wrapMode: Text.WrapAnywhere }
    Description { text: strings.t("settings.modelsPath").arg(form.config.models_directory || ""); wrapMode: Text.WrapAnywhere }
    Description { text: strings.t("settings.ocr"); Layout.topMargin: Style.space(4); Layout.bottomMargin: Style.space(12) }
  }
}
