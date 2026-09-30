// zenith.app: a native Mac window around the dashboard served locally.
// The server runs on its own (a LaunchAgent, see scripts/mac/install.sh); the app wakes it up if it sleeps.
//
// The window is one piece, like a native app: a translucent sidebar (the window's vibrancy
// shows through the page's transparent sidebar), a unified title bar with the traffic lights
// over it, and the system's light or dark appearance. The page marks its title bars; a press
// there drags the window (see src/components/shell/mac.ts).

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

let waitingPage = """
<!doctype html><html><head><meta charset="utf-8"><meta name="color-scheme" content="light dark"><style>
html,body{margin:0;height:100%;background:transparent;color:#71717b;font:13px -apple-system,system-ui;display:grid;place-items:center;-webkit-user-select:none}
.spin{width:18px;height:18px;margin:0 auto 14px;border-radius:50%;border:2px solid rgba(128,128,128,.25);border-top-color:#71717b;animation:s .8s linear infinite}
@keyframes s{to{transform:rotate(360deg)}}p{text-align:center;margin:4px 0}b{font-weight:600;color:CanvasText}
</style></head><body><div><div class="spin"></div><p><b>TITLE</b></p><p>MESSAGE</p></div></body></html>
"""

/// The page's web view. It remembers the last mouse down, so a drag the page asks for a
/// moment later still starts from the right event.
final class ShellWebView: WKWebView {
    var lastMouseDown: NSEvent?
    override func mouseDown(with event: NSEvent) {
        lastMouseDown = event
        super.mouseDown(with: event)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
    var window: NSWindow!
    var webView: ShellWebView!
    var attempts = 0
    var loaded = false
    var knownDown: Set<String> = []
    var firstStatus = true
    let apple = AppleSensors()

    func applicationDidFinishLaunching(_ note: Notification) {
        buildMenu()

        let config = WKWebViewConfiguration()
        config.applicationNameForUserAgent = "ZenithMac/2.0"
        config.websiteDataStore = .default()
        config.userContentController.add(self, name: "zenithShell")
        webView = ShellWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.setValue(false, forKey: "drawsBackground")
        webView.underPageBackgroundColor = .clear
        webView.allowsBackForwardNavigationGestures = true
        webView.allowsMagnification = false

        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1440, height: 920),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        window.title = "zenith"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.titlebarSeparatorStyle = .none
        // An empty unified toolbar: a 52 pt title bar with the traffic lights centered in it,
        // the height of the page's own top bars.
        let toolbar = NSToolbar(identifier: "zenith")
        window.toolbar = toolbar
        window.toolbarStyle = .unified
        window.minSize = NSSize(width: 480, height: 520)
        window.delegate = self
        window.tabbingMode = .disallowed

        // The window's material: seen through the page wherever it is transparent (the sidebar).
        let material = NSVisualEffectView()
        material.material = .sidebar
        material.blendingMode = .behindWindow
        material.state = .followsWindowActiveState
        webView.frame = material.bounds
        webView.autoresizingMask = [.width, .height]
        material.addSubview(webView)
        window.contentView = material
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

    // MARK: - Full screen: the traffic lights go away, and the page's inset with them

    func windowWillEnterFullScreen(_ notification: Notification) { setPageFlag("fullscreen", true) }
    func windowDidExitFullScreen(_ notification: Notification) { setPageFlag("fullscreen", false) }

    func setPageFlag(_ name: String, _ on: Bool) {
        let js = on ? "document.documentElement.dataset.\(name)='1'" : "delete document.documentElement.dataset.\(name)"
        webView.evaluateJavaScript(js)
    }

    /// Menus the page handles itself (src/components/shell/shell-events.tsx).
    func sendToPage(_ action: String) {
        webView.evaluateJavaScript("window.dispatchEvent(new CustomEvent('zenith:menu',{detail:'\(action)'}))")
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

    // MARK: - Local server

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
        if !loaded { webView.loadHTMLString(waitingPage.replacingOccurrences(of: "TITLE", with: "zenith").replacingOccurrences(of: "MESSAGE", with: message), baseURL: nil) }
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

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        if window.styleMask.contains(.fullScreen) { setPageFlag("fullscreen", true) }
    }

    /// The page's requests: move or zoom the window from its title bars. Main frame only;
    /// zenith code's frame goes through the page (src/components/code/code-host.tsx).
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        let origin = message.frameInfo.securityOrigin
        guard message.frameInfo.isMainFrame, origin.host == dashboardURL.host, origin.port == dashboardURL.port ?? 80,
              let body = message.body as? [String: Any], let type = body["type"] as? String else { return }
        switch type {
        case "drag":
            guard let event = NSApp.currentEvent.flatMap({ [.leftMouseDown, .leftMouseDragged].contains($0.type) ? $0 : nil }) ?? webView.lastMouseDown,
                  NSEvent.pressedMouseButtons & 1 == 1 else { return }
            window.performDrag(with: event)
        case "zoom":
            // What a double-click on a title bar does, as set in System Settings → Desktop & Dock.
            switch UserDefaults.standard.string(forKey: "AppleActionOnDoubleClick") {
            case "Minimize": window.miniaturize(nil)
            case "None": break
            default: window.zoom(nil)
            }
        default: break
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { retry() }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { retry() }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { webView.reload() }

    func retry() {
        loaded = false
        attempts = 0
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self.connect() }
    }

    // Attachments in zenith code: files, images, folders.
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = parameters.allowsDirectories
        panel.canChooseFiles = true
        panel.beginSheetModal(for: window) { completionHandler($0 == .OK ? panel.urls : nil) }
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.beginSheetModal(for: window) { _ in completionHandler() }
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: L("Annuler", "Cancel"))
        alert.beginSheetModal(for: window) { completionHandler($0 == .alertFirstButtonReturn) }
    }

    // MARK: - Menus

    @objc func openSettings() { sendToPage("settings") }
    @objc func toggleSidebar() { sendToPage("sidebar") }
    @objc func search() { sendToPage("search") }
    @objc func ask() { sendToPage("ask") }
    @objc func goHome() { sendToPage("home") }
    @objc func refreshData() { sendToPage("refresh") }
    @objc func reload() { webView.reload() }
    @objc func back() { webView.goBack() }
    @objc func forward() { webView.goForward() }
    @objc func zoomIn() { webView.pageZoom = min(webView.pageZoom + 0.1, 2) }
    @objc func zoomOut() { webView.pageZoom = max(webView.pageZoom - 0.1, 0.5) }
    @objc func zoomReset() { webView.pageZoom = 1 }
    @objc func openInBrowser() { NSWorkspace.shared.open(webView.url ?? dashboardURL) }

    func buildMenu() {
        let main = NSMenu()

        let app = NSMenu()
        app.addItem(withTitle: L("À propos de zenith", "About zenith"), action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: L("Réglages…", "Settings…"), action: #selector(openSettings), keyEquivalent: ",")
        app.addItem(.separator())
        let services = NSMenu()
        app.addItem(withTitle: L("Services", "Services"), action: nil, keyEquivalent: "").submenu = services
        NSApp.servicesMenu = services
        app.addItem(.separator())
        app.addItem(withTitle: L("Masquer zenith", "Hide zenith"), action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        app.addItem(withTitle: L("Masquer les autres", "Hide Others"), action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h").keyEquivalentModifierMask = [.command, .option]
        app.addItem(withTitle: L("Tout afficher", "Show All"), action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: L("Quitter zenith", "Quit zenith"), action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        main.addItem(submenu(app, "zenith"))

        let file = NSMenu(title: L("Fichier", "File"))
        // ⌘J and ⌘K belong to the page (and to zenith code inside it): no key equivalents here.
        file.addItem(withTitle: L("Demander à zenith…", "Ask zenith…"), action: #selector(ask), keyEquivalent: "")
        file.addItem(withTitle: L("Rechercher…", "Search…"), action: #selector(search), keyEquivalent: "")
        file.addItem(.separator())
        file.addItem(withTitle: L("Fermer la fenêtre", "Close Window"), action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        main.addItem(submenu(file, L("Fichier", "File")))

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
        view.addItem(withTitle: L("Afficher/masquer la barre latérale", "Toggle Sidebar"), action: #selector(toggleSidebar), keyEquivalent: "s").keyEquivalentModifierMask = [.command, .control]
        view.addItem(.separator())
        view.addItem(withTitle: L("Actualiser les données", "Refresh Data"), action: #selector(refreshData), keyEquivalent: "r")
        view.addItem(withTitle: L("Recharger la page", "Reload Page"), action: #selector(reload), keyEquivalent: "R")
        view.addItem(.separator())
        view.addItem(withTitle: L("Agrandir", "Zoom In"), action: #selector(zoomIn), keyEquivalent: "+")
        view.addItem(withTitle: L("Réduire", "Zoom Out"), action: #selector(zoomOut), keyEquivalent: "-")
        view.addItem(withTitle: L("Taille réelle", "Actual Size"), action: #selector(zoomReset), keyEquivalent: "0")
        view.addItem(.separator())
        view.addItem(withTitle: L("Plein écran", "Enter Full Screen"), action: #selector(NSWindow.toggleFullScreen(_:)), keyEquivalent: "f").keyEquivalentModifierMask = [.command, .control]
        main.addItem(submenu(view, L("Présentation", "View")))

        let go = NSMenu(title: L("Aller", "Go"))
        go.addItem(withTitle: L("Accueil", "Home"), action: #selector(goHome), keyEquivalent: "H").keyEquivalentModifierMask = [.command, .shift]
        go.addItem(withTitle: L("Précédent", "Back"), action: #selector(back), keyEquivalent: "[")
        go.addItem(withTitle: L("Suivant", "Forward"), action: #selector(forward), keyEquivalent: "]")
        go.addItem(.separator())
        go.addItem(withTitle: L("Ouvrir dans le navigateur", "Open in Browser"), action: #selector(openInBrowser), keyEquivalent: "o").keyEquivalentModifierMask = [.command, .shift]
        main.addItem(submenu(go, L("Aller", "Go")))

        let win = NSMenu(title: L("Fenêtre", "Window"))
        win.addItem(withTitle: L("Placer dans le Dock", "Minimize"), action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        win.addItem(withTitle: L("Réduire/agrandir", "Zoom"), action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
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
