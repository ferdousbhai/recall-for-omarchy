import QtQuick
import qs.Commons as Theme

Canvas {
  id: icon
  property string name: "search"
  property color ink: Theme.Color.menu.text
  implicitWidth: 20
  implicitHeight: 20
  onNameChanged: requestPaint()
  onInkChanged: requestPaint()
  onWidthChanged: requestPaint()
  onHeightChanged: requestPaint()
  onPaint: {
    var c = getContext("2d")
    c.reset()
    c.scale(width / 24, height / 24)
    c.strokeStyle = ink
    c.lineWidth = 1.6
    c.lineCap = "round"
    c.lineJoin = "round"
    function line(x, y, a, b) { c.moveTo(x, y); c.lineTo(a, b) }
    c.beginPath()
    if (name === "search") {
      c.arc(10.5, 10.5, 6.5, 0, Math.PI * 2); line(15.5, 15.5, 21, 21)
    } else if (name === "close") {
      line(6, 6, 18, 18); line(18, 6, 6, 18)
    } else if (name === "pause") {
      line(9, 5, 9, 19); line(15, 5, 15, 19)
    } else if (name === "play" || name === "video") {
      c.moveTo(8, 5); c.lineTo(19, 12); c.lineTo(8, 19); c.closePath()
    } else if (name === "settings") {
      for (var i = 0; i < 3; i++) {
        var y = 6 + i * 6; var x = i === 1 ? 15 : 9
        line(4, y, x - 2, y); line(x + 2, y, 20, y)
        c.moveTo(x + 2, y); c.arc(x, y, 2, 0, Math.PI * 2)
      }
    } else if (name === "folder") {
      c.moveTo(3, 6); c.lineTo(10, 6); c.lineTo(12, 8); c.lineTo(21, 8); c.lineTo(21, 19); c.lineTo(3, 19); c.closePath()
    } else if (name === "code") {
      line(8, 6, 3, 12); line(3, 12, 8, 18); line(16, 6, 21, 12); line(21, 12, 16, 18); line(14, 4, 10, 20)
    } else if (name === "audio") {
      line(5, 10, 5, 14); line(10, 5, 10, 19); line(15, 8, 15, 16); line(20, 10, 20, 14)
    } else if (name === "image" || name === "screenshot") {
      c.rect(3, 4, 18, 16); line(3, 17, 9, 11); line(9, 11, 15, 17); line(15, 17, 18, 14); line(18, 14, 21, 17)
      c.moveTo(17, 8); c.arc(16, 8, 1, 0, Math.PI * 2)
    } else if (name === "arrow") {
      line(5, 12, 19, 12); line(14, 7, 19, 12); line(19, 12, 14, 17)
    } else {
      c.moveTo(5, 3); c.lineTo(14, 3); c.lineTo(19, 8); c.lineTo(19, 21); c.lineTo(5, 21); c.closePath()
      line(14, 3, 14, 8); line(14, 8, 19, 8); line(8, 12, 16, 12); line(8, 16, 14, 16)
    }
    c.stroke()
  }
}
