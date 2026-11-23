import QtQuick
import qs.Commons as Commons

// The panel's colours, all taken from the theme's menu surface so a theme that styles the Omarchy menu styles Recall
// too, light themes included.
QtObject {
  // The shell declares `menu` as a plain QtObject, which hides its members from qmllint. Reading it through a var
  // once here keeps every other file fully checked.
  readonly property var menu: Commons.Color.menu
  readonly property color background: menu.background
  readonly property color text: menu.text
  readonly property color accent: menu.selectedText
  readonly property color selection: menu.selectedBackground
  readonly property color scrim: menu.scrim

  // The text colour at a given strength, for secondary text, hairlines and fills.
  function dim(strength: real): color {
    return Qt.alpha(text, strength)
  }

  function tint(strength: real): color {
    return Qt.alpha(accent, strength)
  }
}
