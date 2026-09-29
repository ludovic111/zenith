// zenith.app: a native window showing the dashboard served locally.
// The server runs on its own (a LaunchAgent, see scripts/mac/install.sh); the app wakes it up if it sleeps.

import AppKit
import Contacts
import EventKit
import UserNotifications
import WebKit

let dashboardURL = URL(string: "http://127.0.0.1:4747/")!
/// LaunchAgent label, written into Info.plist by install.sh.
let agentLabel = Bundle.main.object(forInfoDictionaryKey: "ZenithAgentLabel") as? String ?? "dev.zenith.app"
/// French when the Mac speaks French, English otherwise.
let french = Locale.preferredLanguages.first?.hasPrefix("fr") ?? false
func L(_ fr: String, _ en: String) -> String { french ? fr : en }
let night = NSColor(srgbRed: 7 / 255, green: 6 / 255, blue: 13 / 255, alpha: 1)

let waitingPage = """
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;height:100%;background:#07060d;color:#b9b4c9;font:15px -apple-system,system-ui;display:grid;place-items:center}
.sun{width:72px;height:72px;border-radius:50%;margin:0 auto 22px;background:radial-gradient(circle at 40% 35%,#fff6d8,#ffd166 40%,#ff8a4c 80%,#e5418a);
box-shadow:0 0 70px #ffd16688;animation:b 1.8s ease-in-out infinite}
@keyframes b{50%{transform:scale(1.12);box-shadow:0 0 110px #ffd166aa}}
p{text-align:center;margin:6px 0}small{color:#7c7791}
</style></head><body><div><div class="sun"></div><p>TITLE</p><p><small>MESSAGE</small></p></div></body></html>
"""

final class AppDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate, WKUIDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var attempts = 0
    var loaded = false
    var knownDown: Set<String> = []
    var firstStatus = true
    let apple = AppleSensors()

    func applicationDidFinishLaunching(_ note: Notification) {
        buildMenu()

        let config = WKWebViewConfiguration()
        config.applicationNameForUserAgent = "ZenithMac/1.0"
        config.websiteDataStore = .default()
        webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.setValue(false, forKey: "drawsBackground")
        webView.allowsBackForwardNavigationGestures = true

        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1440, height: 920),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: false)
        window.title = "zenith"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.backgroundColor = night
        window.appearance = NSAppearance(named: .darkAqua)
        window.minSize = NSSize(width: 420, height: 520)
        window.contentView = webView
        window.center()
        window.setFrameAutosaveName("ZenithMain")
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        connect()

        // Watch: Dock badge and a notification when a service goes down or comes back.
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
        Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { _ in self.pollStatus() }
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) { self.pollStatus() }

        // Apple sensors: Calendar, Reminders, Mail, Contacts and music, read every 5 minutes;
        // screen time and music are sampled continuously in between.
        apple.start()
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { self.apple.collect() }
        Timer.scheduledTimer(withTimeInterval: 300, repeats: true) { _ in self.apple.collect() }
    }

    // Closing the window keeps zenith in the Dock; clicking the icon reopens it.
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { false }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { window.makeKeyAndOrderFront(nil) }
        return true
    }

    // MARK: - Service watch

    struct Status: Decodable {
        struct Down: Decodable { let project: String; let label: String; let url: String }
        let down: [Down]
        let liveAgents: Int
    }

    func pollStatus() {
        var req = URLRequest(url: dashboardURL.appendingPathComponent("api/status"))
        req.timeoutInterval = 20
        URLSession.shared.dataTask(with: req) { data, _, _ in
            guard let data, let status = try? JSONDecoder().decode(Status.self, from: data) else { return }
            DispatchQueue.main.async { self.apply(status) }
        }.resume()
    }

    func apply(_ status: Status) {
        NSApp.dockTile.badgeLabel = status.down.isEmpty ? nil : String(status.down.count)
        let now = Set(status.down.map { $0.url })
        if !firstStatus {
            for d in status.down where !knownDown.contains(d.url) {
                notify(L("\(d.project) ne répond plus", "\(d.project) is down"), L("\(d.label) est hors ligne.", "\(d.label) is offline."))
            }
            for url in knownDown.subtracting(now) {
                notify(L("De retour", "Back up"), L("\(URL(string: url)?.host ?? url) répond à nouveau.", "\(URL(string: url)?.host ?? url) is answering again."))
            }
        }
        knownDown = now
        firstStatus = false
    }

    func notify(_ title: String, _ body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
    }

    // MARK: - Connexion au serveur local

    func connect() {
        var req = URLRequest(url: dashboardURL.appendingPathComponent("api/health"))
        req.timeoutInterval = 2
        URLSession.shared.dataTask(with: req) { _, response, _ in
            DispatchQueue.main.async {
                if (response as? HTTPURLResponse)?.statusCode == 200 {
                    self.loaded = true
                    self.webView.load(URLRequest(url: dashboardURL))
                } else {
                    self.wakeServer()
                }
            }
        }.resume()
    }

    func wakeServer() {
        attempts += 1
        if attempts == 1 {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
            p.arguments = ["kickstart", "gui/\(getuid())/\(agentLabel)"]
            try? p.run()
        }
        let message = attempts > 40
            ? L("Le serveur ne répond pas. Journal : ~/Library/Logs/Zenith/server.log", "The server isn't answering. Log: ~/Library/Logs/Zenith/server.log")
            : L("Démarrage du serveur local", "Starting the local server")
        if !loaded { webView.loadHTMLString(waitingPage.replacingOccurrences(of: "TITLE", with: L("zenith se lève…", "zenith is rising…")).replacingOccurrences(of: "MESSAGE", with: message), baseURL: nil) }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self.connect() }
    }

    // MARK: - Links: the dashboard stays here, everything else opens in the browser

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = action.request.url else { return decisionHandler(.allow) }
        let local = url.host == dashboardURL.host && url.port == dashboardURL.port
        // Frames served from this Mac (zenith code on its own port) stay embedded.
        let localFrame = action.targetFrame.map { !$0.isMainFrame } == true && url.host == dashboardURL.host
        if local || localFrame || url.scheme == "about" || url.scheme == "data" || url.scheme == "blob" {
            decisionHandler(.allow)
        } else {
            NSWorkspace.shared.open(url)
            decisionHandler(.cancel)
        }
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url { NSWorkspace.shared.open(url) }
        return nil
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { retry() }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { retry() }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { webView.reload() }

    func retry() {
        loaded = false
        attempts = 0
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self.connect() }
    }

    // MARK: - Menus (copy/paste, reload, zoom, full screen)

    @objc func reload() { webView.reload() }
    @objc func goHome() { webView.load(URLRequest(url: dashboardURL)) }
    @objc func zoomIn() { webView.pageZoom = min(webView.pageZoom + 0.1, 2) }
    @objc func zoomOut() { webView.pageZoom = max(webView.pageZoom - 0.1, 0.5) }
    @objc func zoomReset() { webView.pageZoom = 1 }
    @objc func openInBrowser() { NSWorkspace.shared.open(webView.url ?? dashboardURL) }

    func buildMenu() {
        let main = NSMenu()

        let app = NSMenu()
        app.addItem(withTitle: L("À propos de zenith", "About zenith"), action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: L("Masquer zenith", "Hide zenith"), action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        app.addItem(withTitle: L("Quitter zenith", "Quit zenith"), action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        main.addItem(submenu(app, "zenith"))

        let edit = NSMenu(title: L("Édition", "Edit"))
        edit.addItem(withTitle: L("Annuler", "Undo"), action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: L("Rétablir", "Redo"), action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: L("Couper", "Cut"), action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: L("Copier", "Copy"), action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: L("Coller", "Paste"), action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: L("Tout sélectionner", "Select All"), action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        main.addItem(submenu(edit, L("Édition", "Edit")))

        let view = NSMenu(title: L("Présentation", "View"))
        view.addItem(withTitle: L("Vue d'ensemble", "Overview"), action: #selector(goHome), keyEquivalent: "0").keyEquivalentModifierMask = [.command, .shift]
        view.addItem(withTitle: L("Recharger", "Reload"), action: #selector(reload), keyEquivalent: "r")
        view.addItem(.separator())
        view.addItem(withTitle: L("Agrandir", "Zoom In"), action: #selector(zoomIn), keyEquivalent: "+")
        view.addItem(withTitle: L("Réduire", "Zoom Out"), action: #selector(zoomOut), keyEquivalent: "-")
        view.addItem(withTitle: L("Taille réelle", "Actual Size"), action: #selector(zoomReset), keyEquivalent: "0")
        view.addItem(.separator())
        view.addItem(withTitle: L("Ouvrir dans le navigateur", "Open in Browser"), action: #selector(openInBrowser), keyEquivalent: "o")
        view.addItem(withTitle: L("Plein écran", "Enter Full Screen"), action: #selector(NSWindow.toggleFullScreen(_:)), keyEquivalent: "f").keyEquivalentModifierMask = [.command, .control]
        main.addItem(submenu(view, L("Présentation", "View")))

        let win = NSMenu(title: L("Fenêtre", "Window"))
        win.addItem(withTitle: L("Placer dans le Dock", "Minimize"), action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        win.addItem(withTitle: L("Fermer", "Close"), action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        main.addItem(submenu(win, L("Fenêtre", "Window")))
        NSApp.windowsMenu = win

        NSApp.mainMenu = main
    }

    func submenu(_ menu: NSMenu, _ title: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.submenu = menu
        return item
    }
}

// MARK: - Capteurs Apple

/// Runs osascript with a timeout; nil if the script couldn't run or was interrupted.
func osascript(_ script: String, timeout: TimeInterval) -> (out: String, err: String, status: Int32)? {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    p.arguments = ["-e", script]
    let outPipe = Pipe(), errPipe = Pipe()
    p.standardOutput = outPipe
    p.standardError = errPipe
    do { try p.run() } catch { return nil }
    let deadline = Date().addingTimeInterval(timeout)
    while p.isRunning && Date() < deadline { Thread.sleep(forTimeInterval: 0.2) }
    if p.isRunning { p.terminate(); return nil }
    let out = String(data: outPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
    let err = String(data: errPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
    return (out, err, p.terminationStatus)
}

func isRunning(_ bundleID: String) -> Bool {
    !NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).isEmpty
}

/// Reads Calendar and Reminders (EventKit), Mail, Music and Spotify (AppleScript) and birthdays
/// (Contacts), measures time spent in each app, then sends it all to the local server.
/// macOS asks for permission the first time; nothing is modified, everything is read-only.
final class AppleSensors {
    let store = EKEventStore()
    let contacts = CNContactStore()
    let iso = ISO8601DateFormatter()
    let screen = ScreenActivity()
    let music = MusicWatcher()
    var busy = false

    func start() {
        screen.start()
        music.start()
    }

    func collect() {
        if busy { return }
        busy = true
        store.requestFullAccessToEvents { calOK, _ in
            self.store.requestFullAccessToReminders { remOK, _ in
                self.readReminders(remOK) { reminders in
                    let calendar = self.readCalendar(calOK)
                    self.readBirthdays { birthdays in
                        DispatchQueue.global(qos: .utility).async {
                            let mail = self.readMail()
                            let screen = DispatchQueue.main.sync { self.screen.snapshot() }
                            self.send([
                                "capturedAt": self.iso.string(from: Date()),
                                "calendar": calendar,
                                "reminders": reminders,
                                "mail": mail,
                                "birthdays": birthdays,
                                "music": self.music.snapshot(),
                                "screen": screen,
                            ])
                            self.busy = false
                        }
                    }
                }
            }
        }
    }

    /// Only each contact's name and birthday: no number, no address.
    func readBirthdays(done: @escaping ([String: Any]) -> Void) {
        contacts.requestAccess(for: .contacts) { ok, _ in
            guard ok else { return done(["authorized": false, "items": []]) }
            DispatchQueue.global(qos: .utility).async {
                var items: [[String: Any]] = []
                let keys = [CNContactGivenNameKey, CNContactFamilyNameKey, CNContactNicknameKey, CNContactBirthdayKey] as [CNKeyDescriptor]
                try? self.contacts.enumerateContacts(with: CNContactFetchRequest(keysToFetch: keys)) { c, stop in
                    guard let b = c.birthday, let month = b.month, let day = b.day else { return }
                    let full = [c.givenName, c.familyName].filter { !$0.isEmpty }.joined(separator: " ")
                    let name = c.nickname.isEmpty ? full : c.nickname
                    if name.isEmpty { return }
                    var item: [String: Any] = ["name": name, "month": month, "day": day]
                    if let year = b.year { item["year"] = year }
                    items.append(item)
                    if items.count >= 2000 { stop.pointee = true }
                }
                done(["authorized": true, "items": items])
            }
        }
    }

    func readCalendar(_ ok: Bool) -> [String: Any] {
        guard ok else { return ["authorized": false, "calendars": [], "events": []] }
        let calendars = store.calendars(for: .event)
        let from = Date().addingTimeInterval(-86400)
        let to = Date().addingTimeInterval(14 * 86400)
        let events = store.events(matching: store.predicateForEvents(withStart: from, end: to, calendars: calendars))
            .sorted { $0.startDate < $1.startDate }
            .prefix(200)
            .map { e -> [String: Any] in
                [
                    "title": e.title ?? L("Sans titre", "Untitled"),
                    "start": iso.string(from: e.startDate),
                    "end": e.endDate.map { iso.string(from: $0) } ?? NSNull(),
                    "allDay": e.isAllDay,
                    "location": e.location ?? NSNull(),
                    "calendar": e.calendar.title,
                ]
            }
        return ["authorized": true, "calendars": calendars.map { $0.title }, "events": Array(events)]
    }

    func readReminders(_ ok: Bool, done: @escaping ([String: Any]) -> Void) {
        guard ok else { return done(["authorized": false, "items": []]) }
        let predicate = store.predicateForIncompleteReminders(withDueDateStarting: nil, ending: nil, calendars: nil)
        store.fetchReminders(matching: predicate) { list in
            let items = (list ?? []).prefix(100).map { r -> [String: Any] in
                [
                    "title": r.title ?? L("Sans titre", "Untitled"),
                    "due": r.dueDateComponents?.date.map { self.iso.string(from: $0) } ?? NSNull(),
                    "list": r.calendar?.title ?? "",
                    "priority": r.priority,
                ]
            }
            done(["authorized": true, "items": Array(items)])
        }
    }

    /// Mail is only queried when already running: zenith never opens it for you.
    func readMail() -> [String: Any] {
        guard isRunning("com.apple.mail") else {
            return ["running": false, "authorized": true, "accounts": [], "recent": []]
        }
        let script = """
        tell application "Mail"
          set out to ""
          repeat with a in accounts
            try
              set n to unread count of mailbox "INBOX" of a
            on error
              set n to -1
            end try
            set out to out & "A" & tab & (name of a) & tab & n & linefeed
          end repeat
          set c to 0
          repeat with m in (messages of inbox whose read status is false)
            set c to c + 1
            if c > 15 then exit repeat
            try
              set out to out & "M" & tab & (name of account of mailbox of m) & tab & (sender of m) & tab & (subject of m) & tab & ((date received of m) as «class isot» as string) & linefeed
            end try
          end repeat
          return out
        end tell
        """
        guard let r = osascript(script, timeout: 45) else {
            return ["running": true, "authorized": true, "error": L("Mail met trop de temps à répondre", "Mail is taking too long to answer"), "accounts": [], "recent": []]
        }
        let (out, err) = (r.out, r.err)
        if r.status != 0 {
            let denied = err.contains("-1743") || err.lowercased().contains("not authorized")
            return ["running": true, "authorized": !denied, "error": denied ? L("Accès à Mail refusé", "Access to Mail denied") : String(err.prefix(200)), "accounts": [], "recent": []]
        }
        var accounts: [[String: Any]] = [], recent: [[String: Any]] = []
        for line in out.split(separator: "\n") {
            let f = line.split(separator: "\t", omittingEmptySubsequences: false).map(String.init)
            if f.first == "A", f.count >= 3 { accounts.append(["name": f[1], "unread": Int(f[2]) ?? -1]) }
            if f.first == "M", f.count >= 5 { recent.append(["account": f[1], "from": f[2], "subject": f[3], "date": f[4]]) }
        }
        return ["running": true, "authorized": true, "accounts": accounts, "recent": recent]
    }

    func send(_ payload: [String: Any]) {
        guard let body = try? JSONSerialization.data(withJSONObject: payload) else { return }
        var req = URLRequest(url: dashboardURL.appendingPathComponent("api/apple"))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("1", forHTTPHeaderField: "X-Zenith-App")
        req.httpBody = body
        URLSession.shared.dataTask(with: req).resume()
    }
}

// MARK: - Screen time

/// Every 20 seconds, notes the frontmost app, unless the screen is locked or nobody
/// touched the keyboard or mouse for 3 minutes. Nothing leaves this Mac except the totals.
final class ScreenActivity {
    let step: TimeInterval = 20
    let key = "screenDays"
    var days: [String: [String: Double]] = [:]
    let dayFormat: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.locale = Locale(identifier: "en_US_POSIX")
        return f
    }()

    func start() {
        days = (UserDefaults.standard.dictionary(forKey: key) as? [String: [String: Double]]) ?? [:]
        Timer.scheduledTimer(withTimeInterval: step, repeats: true) { _ in self.sample() }
    }

    func sample() {
        let idle = CGEventSource.secondsSinceLastEventType(.combinedSessionState, eventType: CGEventType(rawValue: ~0)!)
        let session = CGSessionCopyCurrentDictionary() as? [String: Any]
        let locked = (session?["CGSSessionScreenIsLocked"] as? Bool) ?? false
        guard idle < 180, !locked, let app = NSWorkspace.shared.frontmostApplication?.localizedName else { return }
        let day = dayFormat.string(from: Date())
        days[day, default: [:]][app, default: 0] += step
        for old in days.keys.sorted().dropLast(14) { days[old] = nil }
        UserDefaults.standard.set(days, forKey: key)
    }

    func snapshot() -> [String: Any] {
        let list = days.keys.sorted().map { day -> [String: Any] in
            let apps = (days[day] ?? [:]).sorted { $0.value > $1.value }.map { ["name": $0.key, "seconds": Int($0.value)] }
            return ["date": day, "apps": apps]
        }
        return ["tracking": true, "days": list]
    }
}

// MARK: - Musique

/// Every minute, asks Music and Spotify (only if open) what they are playing.
final class MusicWatcher {
    let queue = DispatchQueue(label: "zenith.music")
    let key = "musicHistory"
    let iso = ISO8601DateFormatter()
    var current: [String: Any]?
    var history: [[String: Any]] = []
    var denied: Set<String> = []

    let players: [(bundle: String, name: String)] = [("com.apple.Music", "Music"), ("com.spotify.client", "Spotify")]

    func start() {
        queue.async { self.history = (UserDefaults.standard.array(forKey: self.key) as? [[String: Any]]) ?? [] }
        Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { _ in self.queue.async { self.poll() } }
        queue.asyncAfter(deadline: .now() + 5) { self.poll() }
    }

    func poll() {
        var playing: [String: Any]?
        for p in players where isRunning(p.bundle) {
            let script = """
            tell application "\(p.name)"
              if player state is playing then return (name of current track) & tab & (artist of current track) & tab & (album of current track)
            end tell
            return ""
            """
            guard let r = osascript(script, timeout: 10) else { continue }
            if r.status != 0 {
                if r.err.contains("-1743") { denied.insert(p.name) }
                continue
            }
            denied.remove(p.name)
            let f = r.out.trimmingCharacters(in: .newlines).split(separator: "\t", omittingEmptySubsequences: false).map(String.init)
            if f.count >= 3, !f[0].isEmpty {
                playing = ["title": f[0], "artist": f[1], "album": f[2], "app": p.name == "Music" ? L("Musique", "Music") : p.name, "at": iso.string(from: Date())]
                break
            }
        }
        current = playing
        guard let now = playing else { return }
        if let last = history.first, last["title"] as? String == now["title"] as? String, last["artist"] as? String == now["artist"] as? String { return }
        history.insert(now, at: 0)
        history = Array(history.prefix(40))
        UserDefaults.standard.set(history, forKey: key)
    }

    func snapshot() -> [String: Any] {
        queue.sync {
            ["current": current ?? NSNull(), "recent": history, "denied": Array(denied), "running": players.filter { isRunning($0.bundle) }.map { $0.name == "Music" ? L("Musique", "Music") : $0.name }]
        }
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
