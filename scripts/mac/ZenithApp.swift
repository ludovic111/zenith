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

/// Top-left origin, like the web page laying it out.
final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

final class AppDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
    var window: NSWindow!
    var webView: WKWebView!
    let assistants = Assistants()
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
        // The dashboard places Claude and ChatGPT in its layout (src/components/assistants).
        config.userContentController.add(self, name: "zenithShell")
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
        let root = FlippedView()
        webView.frame = root.bounds
        webView.autoresizingMask = [.width, .height]
        root.addSubview(webView)
        assistants.host = root
        assistants.dashboard = webView
        window.contentView = root
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

    // A new dashboard page starts without an assistant on top; its page shows one again.
    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) { assistants.hideAll() }

    func applicationDidBecomeActive(_ note: Notification) { assistants.zenithActivated() }
    func applicationDidHide(_ note: Notification) { assistants.zenithHidden() }

    /// Only the dashboard's own page may place assistants.
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        let origin = message.frameInfo.securityOrigin
        guard message.frameInfo.isMainFrame, origin.host == dashboardURL.host, origin.port == dashboardURL.port ?? 80,
              let body = message.body as? [String: Any], body["type"] as? String == "assistant" else { return }
        let id = body["app"] as? String
        if let action = body["action"] as? String, let id {
            assistants.perform(action, on: id, url: (body["url"] as? String).flatMap(URL.init(string:)))
            return
        }
        var rect: NSRect?
        if let r = body["rect"] as? [String: Double], let x = r["x"], let y = r["y"], let w = r["width"], let h = r["height"] {
            let zoom = webView.pageZoom
            rect = NSRect(x: x * zoom, y: y * zoom, width: w * zoom, height: h * zoom)
        }
        assistants.show(id, rect: rect, mode: body["mode"] as? String ?? "desktop", url: (body["url"] as? String).flatMap(URL.init(string:)))
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

    @objc func reload() {
        if let visible = assistants.visible { visible.reload() } else { webView.reload() }
    }
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

// MARK: - Claude and ChatGPT

/// Claude's and ChatGPT's desktop apps, docked in zenith's window. macOS can't put another
/// app's window inside ours, so zenith does the next best thing: it moves the app's real
/// window (Accessibility API) exactly over the area the dashboard's /apps page leaves
/// transparent, keeps it there while zenith moves or resizes, lets clicks through that area,
/// and hides the app when you leave the page. Plugins, connectors and desktop extensions are
/// the app's own. A desktop app that isn't installed, or Accessibility not granted yet: a web
/// view of claude.ai / chatgpt.com takes the same place (web pages can't frame them, a native
/// web view can), signed in through Safari's cookie store.
final class Assistants: NSObject, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate {
    weak var host: NSView?
    weak var dashboard: WKWebView?
    var views: [String: WKWebView] = [:]
    var popups: [WKWebView: NSWindow] = [:]
    var shown: String?
    var docked: DockedApp?
    /// The reserved area, in the content view's coordinates (top-left origin).
    var slot: NSRect = .zero
    var follow: Timer?
    var lastPlaced: CGRect?
    var launchStarted: Date?

    static let homes: [String: URL] = [
        "claude": URL(string: "https://claude.ai/new")!,
        "chatgpt": URL(string: "https://chatgpt.com/")!,
    ]
    /// Where the desktop apps usually live: by name first, then by bundle id.
    static let desktop: [String: (names: [String], bundleIds: [String])] = [
        "claude": (["Claude.app"], ["com.anthropic.claudefordesktop"]),
        "chatgpt": (["ChatGPT.app"], ["com.openai.chat", "com.openai.codex"]),
    ]
    /// Sign-in with Google refuses web views that don't look like Safari.
    static let userAgent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15"

    var visible: WKWebView? { docked == nil ? shown.flatMap { views[$0] } : nil }

    func view(_ id: String) -> WKWebView? {
        if let v = views[id] { return v }
        guard let home = Assistants.homes[id], let host else { return nil }
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .default()
        config.mediaTypesRequiringUserActionForPlayback = []
        let v = WKWebView(frame: .zero, configuration: config)
        v.customUserAgent = Assistants.userAgent
        v.navigationDelegate = self
        v.uiDelegate = self
        v.allowsBackForwardNavigationGestures = true
        v.isHidden = true
        host.addSubview(v)
        v.load(URLRequest(url: home))
        views[id] = v
        return v
    }

    static func findDesktopApp(_ id: String) -> URL? {
        guard let spec = desktop[id] else { return nil }
        let dirs = ["/Applications", NSHomeDirectory() + "/Applications"]
        for name in spec.names {
            for dir in dirs where FileManager.default.fileExists(atPath: dir + "/" + name) {
                return URL(fileURLWithPath: dir + "/" + name)
            }
        }
        for bundleId in spec.bundleIds {
            if let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleId) { return url }
        }
        return nil
    }

    /// `mode`: "desktop" (dock the app, the default) or "web".
    func show(_ id: String?, rect: NSRect?, mode: String = "desktop", url: URL? = nil) {
        guard let id, let rect else {
            hideAll()
            return
        }
        slot = rect
        let wantsDesktop = mode != "web"
        let appURL = wantsDesktop ? Assistants.findDesktopApp(id) : nil
        let trusted = AXIsProcessTrusted()

        if let appURL, trusted {
            for (_, v) in views { v.isHidden = true }
            if docked?.id != id {
                undock(hide: true)
                docked = DockedApp(id: id, url: appURL)
                lastPlaced = nil
                launchStarted = Date()
                docked?.bringUp()
            }
            shown = id
            startFollowing()
            report(id, mode: "desktop", state: lastPlaced == nil ? "launching" : "docked")
            return
        }

        undock(hide: true)
        for (key, v) in views where key != id { v.isHidden = true }
        guard let v = view(id) else { return }
        v.frame = rect
        if let url, Assistants.allowed(url, for: id) { v.load(URLRequest(url: url)) }
        if v.isHidden || shown != id {
            v.isHidden = false
            v.window?.makeFirstResponder(v)
        }
        shown = id
        let state = !wantsDesktop ? "web" : appURL == nil ? "not-installed" : "needs-permission"
        report(id, mode: "web", state: state)
    }

    func hideAll() {
        for (_, v) in views { v.isHidden = true }
        undock(hide: true)
        if shown != nil, let dashboard { dashboard.window?.makeFirstResponder(dashboard) }
        shown = nil
    }

    func perform(_ action: String, on id: String, url: URL?) {
        switch action {
        case "grant":
            AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
            if let pane = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") { NSWorkspace.shared.open(pane) }
            return
        case "detach":
            // Back to a normal window of its own, where it was docked.
            let app = docked
            docked = nil
            stopFollowing()
            app?.running?.activate()
            report(id, mode: "desktop", state: "detached")
            return
        case "focus":
            docked?.running?.activate()
            return
        default: break
        }
        guard let v = views[id] else { return }
        switch action {
        case "back": v.goBack()
        case "forward": v.goForward()
        case "reload": v.reload()
        case "home": if let home = Assistants.homes[id] { v.load(URLRequest(url: home)) }
        case "open": if let url, Assistants.allowed(url, for: id) { v.load(URLRequest(url: url)) }
        case "browser": if let current = v.url { NSWorkspace.shared.open(current) }
        default: break
        }
    }

    /// Tells the page what it is showing, so it can go transparent over a docked app.
    func report(_ id: String, mode: String, state: String) {
        let json = "{\"app\":\"\(id)\",\"mode\":\"\(mode)\",\"state\":\"\(state)\"}"
        dashboard?.window?.isOpaque = state != "docked"
        dashboard?.window?.backgroundColor = state == "docked" ? .clear : night
        dashboard?.evaluateJavaScript("window.dispatchEvent(new CustomEvent('zenith-shell',{detail:\(json)}))")
    }

    // MARK: Docking

    func startFollowing() {
        guard follow == nil else { return }
        follow = Timer.scheduledTimer(withTimeInterval: 1.0 / 30, repeats: true) { [weak self] _ in self?.tick() }
    }

    func stopFollowing() {
        follow?.invalidate()
        follow = nil
        lastPlaced = nil
        dashboard?.window?.ignoresMouseEvents = false
        dashboard?.window?.isOpaque = true
        dashboard?.window?.backgroundColor = night
    }

    func undock(hide: Bool) {
        guard let app = docked else { return }
        docked = nil
        stopFollowing()
        if hide { app.running?.hide() }
    }

    /// The slot in AX coordinates (global, top-left origin of the primary screen).
    func slotOnScreen() -> CGRect? {
        guard let host, let window = host.window, slot.width > 0, slot.height > 0 else { return nil }
        let inWindow = host.convert(slot, to: nil)
        let cocoa = window.convertToScreen(inWindow)
        let top = NSScreen.screens.first?.frame.maxY ?? cocoa.maxY
        return CGRect(x: cocoa.minX, y: top - cocoa.maxY, width: cocoa.width, height: cocoa.height)
    }

    func tick() {
        guard let app = docked, let window = dashboard?.window, let target = slotOnScreen() else { return }
        if window.isMiniaturized || !window.isVisible {
            app.running?.hide()
            return
        }
        guard let w = app.window() else {
            // Launching, or running without a window: ask again every few seconds.
            if let started = launchStarted, Date().timeIntervalSince(started) > 4 {
                launchStarted = Date()
                app.bringUp()
            }
            return
        }
        if lastPlaced != target || app.frame(of: w) != target {
            if app.running?.isHidden == true { app.running?.unhide() }
            app.place(w, target)
            if lastPlaced == nil {
                report(app.id, mode: "desktop", state: "docked")
                app.running?.activate()
            }
            lastPlaced = target
        }
        // Clicks over the slot go to the app underneath; the rest stays zenith's.
        let mouse = NSEvent.mouseLocation
        let cocoaSlot = window.convertToScreen(host!.convert(slot, to: nil))
        window.ignoresMouseEvents = cocoaSlot.contains(mouse)
    }

    /// zenith came back to the front: the docked app must be right under it, not another app.
    func zenithActivated() {
        guard let app = docked, let window = dashboard?.window, let pid = app.running?.processIdentifier else { return }
        let below = CGWindowListCopyWindowInfo([.optionOnScreenBelowWindow, .excludeDesktopElements], CGWindowID(window.windowNumber)) as? [[String: Any]] ?? []
        let target = slotOnScreen() ?? .zero
        let first = below.first { info in
            guard (info[kCGWindowLayer as String] as? Int) == 0,
                  let b = info[kCGWindowBounds as String] as? [String: CGFloat],
                  let r = CGRect(dictionaryRepresentation: b as CFDictionary) else { return false }
            return r.intersects(target)
        }
        if (first?[kCGWindowOwnerPID as String] as? pid_t) != pid { app.running?.activate() }
    }

    func zenithHidden() { docked?.running?.hide() }

    /// Links the dashboard may open in an assistant: its own site only.
    static func allowed(_ url: URL, for id: String) -> Bool {
        guard url.scheme == "https", let home = homes[id]?.host, let host = url.host else { return false }
        return host == home || host.hasSuffix("." + home)
    }

    // Everything web stays in the view (sign-in providers included); other schemes go to macOS.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = action.request.url, let scheme = url.scheme else { return decisionHandler(.allow) }
        if ["https", "http", "about", "blob", "data"].contains(scheme) { return decisionHandler(action.shouldPerformDownload ? .download : .allow) }
        NSWorkspace.shared.open(url)
        decisionHandler(.cancel)
    }

    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse, decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        decisionHandler(response.canShowMIMEType ? .allow : .download)
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) { download.delegate = self }
    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) { download.delegate = self }

    func download(_ download: WKDownload, decideDestinationUsing response: URLResponse, suggestedFilename: String, completionHandler: @escaping (URL?) -> Void) {
        let folder = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask)[0]
        var dest = folder.appendingPathComponent(suggestedFilename)
        let base = dest.deletingPathExtension().lastPathComponent, ext = dest.pathExtension
        var n = 1
        while FileManager.default.fileExists(atPath: dest.path) {
            n += 1
            dest = folder.appendingPathComponent(ext.isEmpty ? "\(base) \(n)" : "\(base) \(n).\(ext)")
        }
        completionHandler(dest)
    }

    func downloadDidFinish(_ download: WKDownload) {}

    /// Sign-in and connector windows (OAuth) are real popups that talk back to their opener;
    /// plain links opening a new tab go to the browser instead.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        let sized = windowFeatures.width != nil || windowFeatures.height != nil
        let blank = action.request.url == nil || action.request.url?.absoluteString == "about:blank" || action.request.url?.absoluteString == ""
        if !sized && !blank, let url = action.request.url {
            NSWorkspace.shared.open(url)
            return nil
        }
        let popup = WKWebView(frame: .zero, configuration: configuration)
        popup.customUserAgent = Assistants.userAgent
        popup.navigationDelegate = self
        popup.uiDelegate = self
        let size = NSSize(width: windowFeatures.width?.doubleValue ?? 520, height: windowFeatures.height?.doubleValue ?? 680)
        let w = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        w.contentView = popup
        w.isReleasedWhenClosed = false
        w.appearance = NSAppearance(named: .darkAqua)
        if let parent = webView.window { w.setFrameTopLeftPoint(NSPoint(x: parent.frame.midX - size.width / 2, y: parent.frame.maxY - 80)) }
        w.makeKeyAndOrderFront(nil)
        popups[popup] = w
        return popup
    }

    func webViewDidClose(_ webView: WKWebView) {
        popups.removeValue(forKey: webView)?.close()
    }

    // Attachments: files, images, PDFs.
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = parameters.allowsDirectories
        panel.canChooseFiles = true
        if let window = webView.window {
            panel.beginSheetModal(for: window) { completionHandler($0 == .OK ? panel.urls : nil) }
        } else {
            completionHandler(panel.runModal() == .OK ? panel.urls : nil)
        }
    }

    // Voice modes: the microphone, after macOS's own prompt.
    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin, initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType, decisionHandler: @escaping (WKPermissionDecision) -> Void) {
        decisionHandler(type == .microphone ? .grant : .prompt)
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.runModal()
        completionHandler()
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: L("Annuler", "Cancel"))
        completionHandler(alert.runModal() == .alertFirstButtonReturn)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { webView.reload() }
}

/// One desktop app's main window, driven through the Accessibility API.
final class DockedApp {
    let id: String
    let url: URL
    let bundleId: String?

    init(id: String, url: URL) {
        self.id = id
        self.url = url
        bundleId = Bundle(url: url)?.bundleIdentifier
    }

    var running: NSRunningApplication? {
        bundleId.flatMap { NSRunningApplication.runningApplications(withBundleIdentifier: $0).first }
    }

    /// Launches it, or asks it to reopen its window, without stealing the focus yet.
    func bringUp() {
        running?.unhide()
        let config = NSWorkspace.OpenConfiguration()
        config.activates = false
        NSWorkspace.shared.openApplication(at: url, configuration: config) { _, _ in }
    }

    func window() -> AXUIElement? {
        guard let pid = running?.processIdentifier else { return nil }
        let app = AXUIElementCreateApplication(pid)
        var value: CFTypeRef?
        if AXUIElementCopyAttributeValue(app, kAXMainWindowAttribute as CFString, &value) == .success, let value {
            return (value as! AXUIElement)
        }
        guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
              let list = value as? [AXUIElement] else { return nil }
        return list.first { w in
            var role: CFTypeRef?
            AXUIElementCopyAttributeValue(w, kAXSubroleAttribute as CFString, &role)
            return (role as? String) == kAXStandardWindowSubrole as String
        }
    }

    func frame(of w: AXUIElement) -> CGRect? {
        var pos: CFTypeRef?, size: CFTypeRef?
        guard AXUIElementCopyAttributeValue(w, kAXPositionAttribute as CFString, &pos) == .success,
              AXUIElementCopyAttributeValue(w, kAXSizeAttribute as CFString, &size) == .success else { return nil }
        var p = CGPoint.zero, s = CGSize.zero
        AXValueGetValue(pos as! AXValue, .cgPoint, &p)
        AXValueGetValue(size as! AXValue, .cgSize, &s)
        return CGRect(origin: p, size: s)
    }

    func place(_ w: AXUIElement, _ r: CGRect) {
        AXUIElementSetAttributeValue(w, kAXMinimizedAttribute as CFString, kCFBooleanFalse)
        var p = r.origin, s = r.size
        guard let pos = AXValueCreate(.cgPoint, &p), let size = AXValueCreate(.cgSize, &s) else { return }
        // Position, size, position again: a resize can nudge the origin on the way.
        AXUIElementSetAttributeValue(w, kAXPositionAttribute as CFString, pos)
        AXUIElementSetAttributeValue(w, kAXSizeAttribute as CFString, size)
        AXUIElementSetAttributeValue(w, kAXPositionAttribute as CFString, pos)
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
