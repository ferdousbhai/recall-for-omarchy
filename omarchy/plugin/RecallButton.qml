import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons as Theme

Button {
  id: control
  property string iconName: ""
  property bool primary: false
  property bool compact: false
  property string hint: text
  hoverEnabled: true
  implicitHeight: 34
  implicitWidth: compact ? 34 : contentItem.implicitWidth + 24
  padding: compact ? 7 : 12
  Accessible.name: hint
  ToolTip.visible: hovered && hint.length > 0
  ToolTip.text: hint
  ToolTip.delay: 600
  background: Rectangle {
    radius: 6
    color: control.down ? Qt.alpha(Theme.Color.menu.text, 0.16) : control.primary ? Qt.alpha(Theme.Color.menu.selectedText, 0.14) : control.hovered || control.visualFocus ? Qt.alpha(Theme.Color.menu.text, 0.07) : "transparent"
    border.width: control.visualFocus || control.primary ? 1 : 0
    border.color: Qt.alpha(Theme.Color.menu.selectedText, control.visualFocus ? 0.8 : 0.35)
    Behavior on color { ColorAnimation { duration: 100 } }
  }
  contentItem: RowLayout {
    spacing: 7
    RecallIcon { visible: control.iconName.length > 0; name: control.iconName; ink: control.primary ? Theme.Color.menu.selectedText : Theme.Color.menu.text; Layout.preferredWidth: 17; Layout.preferredHeight: 17 }
    Text { visible: !control.compact; text: control.text; color: control.primary ? Theme.Color.menu.selectedText : Theme.Color.menu.text; font.pixelSize: 12; font.weight: Font.Medium; font.family: Theme.Style.fontFamily }
  }
}
