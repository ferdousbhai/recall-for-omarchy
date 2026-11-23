import QtQuick
import qs.Commons as Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "ferdous.recall"
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  // The bar hands itself over as a plain QtObject; a var lets its run() be called without qmllint guessing.
  readonly property var host: root.bar

  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰍉"
    tooltipText: strings.t("widget.tooltip")
    horizontalMargin: 8
    onPressed: function(mouseButton) {
      // The panel belongs to the shell's panel loader, not to this widget, so one panel serves every monitor's
      // bar and the hotkey works with the widget removed.
      if (root.host) root.host.run("recall-shell shell toggle ferdous.recall '{}'")
    }
  }
}
