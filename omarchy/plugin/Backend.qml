import Quickshell
import Quickshell.Io
import QtQuick

// The panel's one connection to the Recall daemon: a long-lived `recall client` process that carries JSON lines to
// the daemon's socket and back. Everything the daemon says is kept here as plain state (never as finished sentences),
// so the panel can word it in the current language and re-word it when the language changes.
Item {
  id: backend
  visible: false

  // True while the panel is on screen. Status is only asked for then: an unseen panel starts no timers.
  property bool watching: false
  property string clientPath: Quickshell.env("HOME") + "/.local/bin/recall-client"

  property var hits: []
  property bool searching: false
  property bool paused: false
  // The daemon's last status reply, or null before the first one.
  property var info: null
  // "starting", "ready", "missing" (the client launcher isn't installed), "unavailable" (the client runs but the
  // daemon's socket doesn't answer) or "stopped" (the client exited unexpectedly).
  property string link: "starting"
  // What the last reply was about: "", "saved", "error", "unreadable", "semantic", "warming" or "exactOnly".
  property string noticeKind: ""
  property string noticeDetail: ""

  property int serial: 0
  property int latestSearch: -1
  property int pendingSave: -1
  // Both delays double on each failure and reset on the first good reply, so a missing binary or a stopped
  // service is retried within seconds at first and about once a minute after that, instead of every 2 s forever.
  readonly property int firstRetry: 2000
  readonly property int slowestRetry: 60000
  readonly property int firstPoll: 3000
  readonly property int slowestPoll: 30000
  property int retryDelay: firstRetry
  property int pollDelay: firstPoll

  signal configReceived(var config, bool saved)

  function send(op: string, data: var): int {
    if (!transport.running) return -1
    var message = data || {}
    message.op = op
    message.id = ++serial
    transport.write(JSON.stringify(message) + "\n")
    return message.id
  }

  function setNotice(kind: string, detail: string) {
    noticeKind = kind
    noticeDetail = detail
  }

  function search(text: string) {
    if (!text.trim()) {
      hits = []
      searching = false
      latestSearch = -1
      return
    }
    latestSearch = send("search", {query: text})
    // Without a client there is nothing to wait for; the link state says why.
    searching = latestSearch !== -1
  }

  function save(settings: var) {
    pendingSave = send("configure", {config: settings})
  }

  function setPaused(value: bool) {
    send("pause", {paused: value})
    send("status")
  }

  // Opening the panel is the user asking again, so try now rather than waiting out the backoff.
  function wake() {
    pollDelay = firstPoll
    if (!transport.running) {
      reconnect.stop()
      transport.running = true
    }
  }

  function receive(line: string) {
    var reply
    try {
      reply = JSON.parse(line)
    } catch (error) {
      setNotice("unreadable", "")
      return
    }
    if (!reply || typeof reply !== "object") return
    // `recall client` marks a failure with the same `kind` words as the command line; "unavailable" is the one that
    // means no daemon is listening, as opposed to a request the daemon turned down.
    if (reply.kind === "unavailable") {
      link = "unavailable"
      searching = false
      if (reply.id === pendingSave) pendingSave = -1
      pollDelay = Math.min(pollDelay * 2, slowestPoll)
      return
    }
    link = "ready"
    retryDelay = firstRetry
    pollDelay = firstPoll
    if (reply.error) {
      // The daemon's own message; diagnostics stay in English.
      setNotice("error", String(reply.error))
      searching = false
      if (reply.id === pendingSave) pendingSave = -1
      return
    }
    var saved = reply.id === pendingSave
    if (saved) {
      pendingSave = -1
      setNotice("saved", "")
    }
    if (reply.config !== undefined) configReceived(reply.config, saved)
    if (reply.line !== undefined) {
      info = reply
      paused = reply.paused === true
    }
    if (reply.id === latestSearch && !reply.skipped) {
      hits = Array.isArray(reply.hits) ? reply.hits : []
      searching = false
      if (reply.encoder_error) setNotice("exactOnly", String(reply.encoder_error))
      else setNotice(reply.meaning ? "semantic" : "warming", "")
    }
  }

  Process {
    id: transport
    // Started through sh so a missing launcher is an exit status (127, or 126 when it can't be run) rather than a
    // process that silently never starts: that is what tells "not installed" from "stopped".
    command: ["/bin/sh", "-c", "exec \"$0\"", backend.clientPath]
    running: true
    stdinEnabled: true
    stdout: SplitParser {
      onRead: function(line) { backend.receive(line) }
    }
    // The link only counts as back once the daemon answers, so ask straight away rather than on the next poll.
    onStarted: backend.send("status")
    onExited: function(exitCode, exitStatus) {
      backend.searching = false
      backend.pendingSave = -1
      backend.link = exitCode === 127 || exitCode === 126 ? "missing" : "stopped"
      reconnect.interval = backend.retryDelay
      backend.retryDelay = Math.min(backend.retryDelay * 2, backend.slowestRetry)
      reconnect.restart()
    }
  }

  Timer {
    id: reconnect
    onTriggered: transport.running = true
  }

  // The daemon has no way to push progress, so indexing status is asked for, but only while the panel is shown.
  Timer {
    interval: backend.pollDelay
    running: backend.watching
    repeat: true
    onTriggered: backend.send("status")
  }

  onWatchingChanged: if (watching) wake()
}
