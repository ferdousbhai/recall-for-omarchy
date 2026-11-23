import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import qs.Commons
import qs.Commons as Commons
// Qualified, because the shell's kit has a Button of its own that would replace the Controls one this extends.
import qs.Ui as Ui

Button {
  id: control
  property string iconName: ""
  property bool primary: false
  property bool compact: false
  // For an action that loses something; drawn in the theme's urgent colour.
  property bool danger: false
  readonly property color ink: danger ? Commons.Color.urgent : primary ? theme.accent : theme.text
  // What the tooltip and screen readers say; icon-only buttons have no caption to fall back on.
  property string hint: text
  hoverEnabled: true
  implicitHeight: Style.space(34)
  implicitWidth: compact ? Style.space(34) : content.implicitWidth + Style.space(24)
  horizontalPadding: compact ? Style.space(7) : Style.space(12)
  // The caption is centred in the button's height; equal padding all round would push it below the middle.
  verticalPadding: compact ? Style.space(7) : 0
  Accessible.name: hint
  RecallTheme { id: theme }
  // The shell's own tooltip, so it takes the theme's tooltip colours and corners.
  Ui.PanelToolTip {
    visible: control.hovered && control.hint.length > 0 && (control.compact || control.hint !== control.text)
    text: control.hint
  }
  background: Rectangle {
    radius: Style.cornerRadius
    color: control.down ? theme.dim(0.16) : control.primary ? Qt.alpha(control.ink, 0.14) : control.hovered || control.visualFocus ? theme.dim(0.07) : "transparent"
    border.width: control.visualFocus || control.primary ? Style.spacing.hairline : 0
    border.color: Qt.alpha(control.primary ? control.ink : theme.accent, control.visualFocus ? 0.8 : 0.35)
    Behavior on color { ColorAnimation { duration: Style.duration(100) } }
  }
  contentItem: RowLayout {
    id: content
    spacing: Style.space(7)
    RecallIcon { visible: control.iconName.length > 0; name: control.iconName; ink: control.ink; Layout.preferredWidth: Style.space(17); Layout.preferredHeight: Style.space(17) }
    Text { visible: !control.compact; text: control.text; textFormat: Text.PlainText; color: control.ink; font.pixelSize: Style.font.body; font.weight: Font.Medium; font.family: Style.font.family }
  }
}
