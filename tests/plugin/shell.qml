import QtQuick
import QtTest
import Quickshell
import qs.Commons

// Loads the plugin's real PanelCard and Backend in an ordinary window, with tests/plugin/fake-client standing in for
// `recall client`, and drives one scene with real key events: bin/check-plugin runs every scene offscreen and fails
// on a wrong result or on any warning the plugin's QML logs. The panel's own layer-shell window (SearchPanel.qml)
// can't be created offscreen, which is why what it shows lives in PanelCard.
//
// Environment: RECALL_PLUGIN (the plugin folder), RECALL_CLIENT (the client to start), RECALL_SCENE, and
// RECALL_SHOT (optional: where to save a picture of the scene's last state).
ShellRoot {
  id: root
  readonly property string plugin: Quickshell.env("RECALL_PLUGIN")
  readonly property string client: Quickshell.env("RECALL_CLIENT")
  readonly property string scene: Quickshell.env("RECALL_SCENE")
  readonly property string shot: Quickshell.env("RECALL_SHOT") || ""
  property var backend: null
  property var card: null
  property int closes: 0
  property var steps: []
  property int at: 0
  property int waited: 0

  function fail(message) {
    console.log("HARNESS_FAIL " + root.scene + ": " + message)
    Qt.quit()
  }
  function equal(actual, expected, what) {
    if (JSON.stringify(actual) !== JSON.stringify(expected))
      throw new Error(what + ": expected " + JSON.stringify(expected) + ", got " + JSON.stringify(actual))
  }
  function find(name, from) {
    var item = from || root.card
    if (item.objectName === name) return item
    var children = item.children || []
    for (var i = 0; i < children.length; i++) {
      var found = root.find(name, children[i])
      if (found) return found
    }
    return null
  }
  function text(name) {
    var item = root.find(name)
    if (!item) throw new Error("nothing is named " + name)
    return item.text
  }
  function press(key, modifiers) { events.keyClick(key, modifiers || Qt.NoModifier, -1) }
  function type(characters) {
    for (var i = 0; i < characters.length; i++) events.keyClickChar(characters.charAt(i), Qt.NoModifier, -1)
  }
  // The field holding the keyboard, by the id-like names the form gives its inputs' placeholders.
  function focused() { return host.Window.activeFocusItem }

  // A scene is a list of steps: `run` does something and checks its result at once, `until` waits for the panel to
  // catch up (a reply from the client, the search debounce) and fails the scene if it never does.
  function next() {
    if (root.at >= root.steps.length) return root.finish()
    var step = root.steps[root.at]
    try {
      if (step.until) {
        if (!step.until()) {
          if (++root.waited > 120) return root.fail("still waiting for: " + step.what)
          return tick.restart()
        }
      } else {
        step.run()
      }
    } catch (error) {
      return root.fail(String(error.message || error))
    }
    root.at++
    root.waited = 0
    Qt.callLater(root.next)
  }
  function finish() {
    if (root.shot === "") {
      console.log("HARNESS_OK " + root.scene)
      return Qt.quit()
    }
    host.grabToImage(function(result) {
      result.saveToFile(root.shot)
      console.log("HARNESS_OK " + root.scene)
      Qt.quit()
    })
  }

  readonly property var ready: ({until: function() { return root.backend.link === "ready" && root.backend.info !== null }, what: "the first status reply"})
  readonly property var settingsLoaded: ({until: function() { return root.card.settings.loaded && root.focused() !== null && root.focused().placeholderText !== undefined }, what: "the settings to load and take the keyboard"})
  // Lets a colour fade finish before a picture is taken.
  function settle() { var ticks = 0; return {until: function() { return ++ticks > 6 }, what: "animations to finish"} }
  function openSettings() { return {run: function() { root.card.present({settings: true}) }} }
  function openSearch() { return {run: function() { root.card.present({}) }} }

  readonly property var scenes: ({
    // Typing searches after the debounce, counts results through the plural form, and the arrows move the selection.
    "search": [
      root.openSearch(), root.ready,
      {run: function() {
        root.equal(root.text("recallStatus"), "1204 files indexed", "status line")
        root.type("lease")
      }},
      {until: function() { return root.backend.hits.length === 3 }, what: "three results"},
      {run: function() {
        root.equal(root.text("recallCount"), "3 results", "count")
        root.press(Qt.Key_Down)
        root.equal(root.card.selected, 1, "selection after Down")
        root.press(Qt.Key_Up)
        root.press(Qt.Key_Up)
        root.equal(root.card.selected, 0, "selection stops at the top")
      }}
    ],
    "one-result": [
      root.openSearch(), root.ready,
      {run: function() { root.type("one") }},
      {until: function() { return root.backend.hits.length === 1 }, what: "one result"},
      {run: function() { root.equal(root.text("recallCount"), "1 result", "singular count") }}
    ],
    "no-results": [
      root.openSearch(), root.ready,
      {run: function() { root.type("none") }},
      {until: function() { return root.backend.noticeKind === "semantic" }, what: "the empty reply"},
      {run: function() {
        root.equal(root.text("recallCount"), "0 results", "zero count")
        root.equal(root.text("recallEmptyTitle"), "No matches found", "empty state")
      }}
    ],
    // Tab leaves a path box for the next one instead of typing a tab into it, and Shift+Tab comes back.
    "settings-tab": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() {
        var first = root.focused()
        root.equal(first.text, "/home/you/Documents\n/home/you/Pictures", "the first field holds the search folders")
        root.press(Qt.Key_Tab)
        root.equal(first.text, "/home/you/Documents\n/home/you/Pictures", "Tab typed nothing into the field")
        if (root.focused() === first) throw new Error("Tab did not move the keyboard out of the first path box")
        root.equal(root.focused().placeholderText, "/home/you/projects", "Tab went to the code folders")
        root.press(Qt.Key_Backtab, Qt.ShiftModifier)
        if (root.focused() !== first) throw new Error("Shift+Tab did not come back")
        for (var i = 0; i < 5; i++) root.press(Qt.Key_Tab)
        if (root.focused().placeholderText !== undefined) throw new Error("five Tabs should have left the text boxes")
      }}
    ],
    // Unsaved settings: Esc asks, dismissing the question keeps the edit, and only "Discard" drops it.
    "settings-discard": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() {
        root.equal(root.card.settings.dirty, false, "a freshly loaded form is clean")
        root.press(Qt.Key_Escape)
        root.equal(root.closes, 1, "Esc closes a clean settings view at once")
        root.card.present({settings: true})
      }},
      root.settingsLoaded,
      {run: function() {
        root.type("x")
        root.equal(root.card.settings.dirty, true, "typing makes the form unsaved")
        root.press(Qt.Key_Backspace)
        root.equal(root.card.settings.dirty, false, "typing it back is clean again")
        root.type("x")
        root.press(Qt.Key_Escape)
        root.equal(root.card.asking, true, "Esc asks before discarding")
        root.equal(root.closes, 1, "the panel stays up while it asks")
        root.press(Qt.Key_Escape)
        root.equal(root.card.asking, false, "Esc dismisses the question")
        root.equal(root.card.settings.dirty, true, "dismissing keeps the edit")
        root.equal(root.card.showSettings, true, "and stays on the settings")
        root.press(Qt.Key_Comma, Qt.ControlModifier)
        root.equal(root.card.asking, true, "leaving for the search view asks too")
        root.press(Qt.Key_Return)
        root.equal(root.card.asking, false, "Enter takes the selected answer")
        root.equal(root.card.settings.dirty, true, "which is Keep editing")
        root.card.requestClose()
        root.equal(root.card.asking, true, "the hotkey's close asks as well")
        root.equal(root.closes, 1, "and does not close yet")
      }}
    ],
    "settings-discard-confirmed": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() {
        root.type("x")
        root.press(Qt.Key_Escape)
        root.press(Qt.Key_Tab)
        root.press(Qt.Key_Return)
        root.equal(root.card.asking, false, "the question is answered")
        root.equal(root.card.settings.dirty, false, "Discard puts the saved settings back")
        root.equal(root.closes, 1, "and the panel closes")
      }}
    ],
    // Ctrl+S saves what is on screen, the daemon's reply becomes the new saved state, and the search view returns.
    "settings-save": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() {
        root.press(Qt.Key_End, Qt.ControlModifier)
        root.press(Qt.Key_Return)
        root.type("/home/you/notes")
        root.equal(root.text("recallNotice"), "Unsaved changes", "the unsaved notice")
        root.press(Qt.Key_S, Qt.ControlModifier)
      }},
      {until: function() { return root.backend.noticeKind === "saved" }, what: "the save to be acknowledged"},
      {run: function() {
        root.equal(root.card.settings.config.roots, ["/home/you/Documents", "/home/you/Pictures", "/home/you/notes"], "saved folders")
        root.equal(root.card.settings.dirty, false, "saved means clean")
        root.equal(root.card.showSettings, false, "back on the search view")
        root.equal(root.text("recallNotice"), "Settings saved", "the saved notice")
      }}
    ],
    "pause": [
      root.openSearch(), root.ready,
      {run: function() { root.press(Qt.Key_P, Qt.ControlModifier) }},
      {until: function() { return root.backend.paused }, what: "indexing to pause"},
      {run: function() {
        root.equal(root.text("recallStatus"), "Indexing paused", "status line")
        root.press(Qt.Key_P, Qt.ControlModifier)
      }},
      {until: function() { return !root.backend.paused }, what: "indexing to resume"}
    ],
    // With no client installed the panel says so, and each retry waits twice as long as the last.
    "missing-client": [
      root.openSearch(),
      {until: function() { return root.backend.link === "missing" }, what: "the missing client to be noticed"},
      {run: function() {
        root.equal(root.text("recallEmptyTitle"), "Recall isn’t set up for this user", "what is wrong")
        root.equal(root.text("recallEmptyDetail").indexOf(root.client) >= 0, true, "the missing file is named")
        root.equal(root.backend.retryDelay, 4000, "the next retry waits longer")
      }},
      {until: function() { return root.backend.retryDelay === 8000 }, what: "the second retry"}
    ],
    // Scenes that only arrange a view for bin/check-plugin --look; they assert nothing, so they work in any language.
    "look-search": [
      root.openSearch(), root.ready,
      {run: function() { root.type("the clause about ending the Globex contract early") }},
      {until: function() { return root.backend.hits.length === 3 }, what: "three results"},
      {run: function() { root.press(Qt.Key_Down) }},
      root.settle()
    ],
    "look-empty": [root.openSearch(), root.ready],
    "look-settings": [root.openSettings(), root.ready, root.settingsLoaded],
    "look-settings-end": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() { for (var i = 0; i < 7; i++) root.press(Qt.Key_Tab) }}
    ],
    "look-discard": [
      root.openSettings(), root.ready, root.settingsLoaded,
      {run: function() { root.type("x"); root.press(Qt.Key_Escape) }}
    ],
    "look-missing": [root.openSearch(), {until: function() { return root.backend.link === "missing" }, what: "the missing client"}],
    "look-down": [root.openSearch(), {until: function() { return root.backend.link === "unavailable" }, what: "the stopped service"}],
    "service-down": [
      root.openSearch(),
      {until: function() { return root.backend.link === "unavailable" }, what: "the stopped service to be noticed"},
      {run: function() {
        root.equal(root.text("recallEmptyTitle"), "The search service isn’t running", "what is wrong")
        root.equal(root.backend.pollDelay > root.backend.firstPoll, true, "status is asked for less often")
      }}
    ]
  })

  FloatingWindow {
    id: window
    implicitWidth: Style.space(960)
    implicitHeight: Style.space(720)
    color: Color.background
    Item {
      id: host
      anchors.fill: parent
      TestEvent { id: events }
    }
  }
  Timer { id: tick; interval: 50; onTriggered: root.next() }
  Timer {
    // Long enough for the window to exist; everything after waits on real conditions.
    interval: 300
    running: true
    onTriggered: {
      var steps = root.scenes[root.scene]
      if (!steps) return root.fail("no such scene")
      var parts = {backend: "Backend.qml", card: "PanelCard.qml"}
      var made = {}
      for (var name in parts) {
        var component = Qt.createComponent("file://" + root.plugin + "/" + parts[name])
        if (component.status !== Component.Ready) return root.fail(component.errorString())
        made[name] = component
      }
      root.backend = made.backend.createObject(host, {clientPath: root.client})
      root.card = made.card.createObject(host, {backend: root.backend, active: true,
        width: Style.space(880), height: Style.space(650)})
      if (!root.backend || !root.card) return root.fail("the panel could not be created")
      root.card.anchors.centerIn = host
      root.backend.watching = true
      root.card.closeRequested.connect(function() { root.closes++ })
      host.Window.window.requestActivate()
      root.steps = [{until: function() { return host.Window.active }, what: "the window to take the keyboard"}].concat(steps)
      root.next()
    }
  }
}
