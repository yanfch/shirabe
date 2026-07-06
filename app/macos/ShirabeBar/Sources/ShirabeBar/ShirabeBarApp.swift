import AppKit
import Foundation
import SwiftUI
import WebKit

private final class MenuPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

@main
final class AppDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate {
    private enum Layout {
        static let panelWidth: CGFloat = 348
        static let preferredPanelHeight: CGFloat = 860
        static let screenInset: CGFloat = 8
        static let panelGap: CGFloat = 8
        static let cornerRadius: CGFloat = 18
    }

    private static var retainedDelegate: AppDelegate?

    private let serverBind = "127.0.0.1:7778"
    private let appURL = URL(string: "http://127.0.0.1:7778/?view=menubar")!
    private let dashboardURL = URL(string: "http://127.0.0.1:7778/?page=usage")!
    private let healthURL = URL(string: "http://127.0.0.1:7778/api/health")!
    private let syncURL = URL(string: "http://127.0.0.1:7778/api/sync")!
    private let sharedWorkspacePath = "/Users/Shared/Shirabe"

    private var statusItem: NSStatusItem?
    private var panel: NSPanel?
    private var webView: WKWebView?
    private var serverProcess: Process?
    private var collectorProcess: Process?
    private var serverLogFile: FileHandle?
    private var collectorLogFile: FileHandle?
    private var outsideClickMonitor: Any?
    private var hasLoadedWebView = false
    private var hasRunInitialCollector = false

    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        retainedDelegate = delegate
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        configureStatusItem()
        configurePanel()
        prepareWebView()
        Task { await ensureServerAndShowContentIfNeeded() }
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let outsideClickMonitor {
            NSEvent.removeMonitor(outsideClickMonitor)
        }
        stopBundledServer()
    }

    private func configureStatusItem() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem = item

        guard let button = item.button else { return }
        button.image = menuBarIcon()
        button.image?.isTemplate = true
        button.target = self
        button.action = #selector(handleStatusItemClick(_:))
        button.sendAction(on: [.leftMouseUp, .rightMouseUp])
    }

    private func menuBarIcon() -> NSImage? {
        if let image = bundledImage(named: "MenuBarIconTemplate") {
            image.size = NSSize(width: 18, height: 18)
            image.isTemplate = true
            image.accessibilityDescription = "Shirabe"
            return image
        }

        let image = NSImage(systemSymbolName: "chart.bar.xaxis", accessibilityDescription: "Shirabe")
        image?.isTemplate = true
        return image
    }

    private func bundledImage(named name: String) -> NSImage? {
        if let url = Bundle.main.url(forResource: name, withExtension: "png"),
           let image = NSImage(contentsOf: url) {
            return image
        }

        #if SWIFT_PACKAGE
        if let url = Bundle.module.url(forResource: name, withExtension: "png"),
           let image = NSImage(contentsOf: url) {
            return image
        }
        #endif

        return nil
    }

    private func configurePanel() {
        let panel = MenuPanel(
            contentRect: NSRect(x: 0, y: 0, width: Layout.panelWidth, height: Layout.preferredPanelHeight),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        panel.backgroundColor = .clear
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        panel.acceptsMouseMovedEvents = true
        panel.hasShadow = true
        panel.hidesOnDeactivate = false
        panel.isFloatingPanel = true
        panel.isOpaque = false
        panel.isReleasedWhenClosed = false
        panel.level = .statusBar
        self.panel = panel
    }

    private func prepareWebView() {
        let configuration = WKWebViewConfiguration()
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.allowsMagnification = false
        webView.navigationDelegate = self
        webView.setValue(false, forKey: "drawsBackground")
        webView.wantsLayer = true
        webView.layer?.cornerRadius = Layout.cornerRadius
        webView.layer?.masksToBounds = true
        self.webView = webView
    }

    @objc private func handleStatusItemClick(_ sender: NSStatusBarButton) {
        if NSApp.currentEvent?.type == .rightMouseUp {
            showContextMenu(from: sender)
            return
        }

        togglePanel()
    }

    private func togglePanel() {
        guard let panel else { return }
        if panel.isVisible {
            closePanel()
        } else {
            openPanel()
        }
    }

    private func openPanel() {
        guard let panel else { return }
        updatePanelFrame()
        if !hasLoadedWebView {
            Task { await ensureServerAndShowContentIfNeeded() }
        }
        NSApp.activate(ignoringOtherApps: true)
        panel.makeKeyAndOrderFront(nil)
        installOutsideClickMonitor()
    }

    private func closePanel() {
        panel?.orderOut(nil)
        removeOutsideClickMonitor()
    }

    private func updatePanelFrame() {
        guard let panel, let button = statusItem?.button, let buttonWindow = button.window else {
            return
        }

        let buttonFrame = buttonWindow.convertToScreen(button.convert(button.bounds, to: nil))
        let screen = buttonWindow.screen ?? NSScreen.main
        let visibleFrame = screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? .zero
        let availableHeight = max(420, visibleFrame.height - Layout.screenInset * 2)
        let panelHeight = min(Layout.preferredPanelHeight, availableHeight)
        let panelSize = NSSize(width: Layout.panelWidth, height: panelHeight)

        let minX = visibleFrame.minX + Layout.screenInset
        let maxX = visibleFrame.maxX - panelSize.width - Layout.screenInset
        let x = min(max(buttonFrame.midX - panelSize.width / 2, minX), maxX)

        let belowY = buttonFrame.minY - panelSize.height - Layout.panelGap
        let y = max(belowY, visibleFrame.minY + Layout.screenInset)

        panel.setFrame(NSRect(origin: NSPoint(x: x, y: y), size: panelSize), display: true)
    }

    private func installOutsideClickMonitor() {
        removeOutsideClickMonitor()
        outsideClickMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in
            self?.closePanel()
        }
    }

    private func removeOutsideClickMonitor() {
        if let outsideClickMonitor {
            NSEvent.removeMonitor(outsideClickMonitor)
            self.outsideClickMonitor = nil
        }
    }

    @MainActor
    private func ensureServerAndShowContentIfNeeded() async {
        await ensureServerAvailable()
        runInitialCollectorIfNeeded()
        await checkServerAndShowContentIfNeeded()
    }

    private func ensureServerAvailable() async {
        if await isServerHealthy() {
            return
        }

        startBundledServerIfPossible()
        for _ in 0..<50 {
            try? await Task.sleep(nanoseconds: 200_000_000)
            if await isServerHealthy() {
                return
            }
        }
    }

    private func startBundledServerIfPossible() {
        if serverProcess?.isRunning == true {
            return
        }

        guard let serverURL = bundledServerURL(),
              let uiDirectoryURL = Bundle.main.resourceURL?.appendingPathComponent("ui", isDirectory: true),
              FileManager.default.fileExists(atPath: uiDirectoryURL.path) else {
            return
        }

        let process = Process()
        process.executableURL = serverURL
        process.arguments = [
            "serve",
            "--bind",
            serverBind,
            "--ui-dir",
            uiDirectoryURL.path,
            "--exit-when-parent-exits",
            String(ProcessInfo.processInfo.processIdentifier),
        ]
        process.environment = serverEnvironment()

        if let logFile = openServerLogFile() {
            process.standardOutput = logFile
            process.standardError = logFile
            serverLogFile = logFile
        }

        process.terminationHandler = { [weak self, weak process] _ in
            DispatchQueue.main.async {
                guard let self, let process, self.serverProcess === process else { return }
                self.serverProcess = nil
            }
        }

        do {
            try process.run()
            serverProcess = process
        } catch {
            serverLogFile?.closeFile()
            serverLogFile = nil
        }
    }

    private func runInitialCollectorIfNeeded() {
        guard !hasRunInitialCollector else { return }
        hasRunInitialCollector = true
        runBundledCollectorIfPossible { [weak self] in
            self?.requestServerSync {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
                    self?.reloadWebView()
                }
            }
        }
    }

    private func runBundledCollectorIfPossible(completion: (() -> Void)? = nil) {
        if collectorProcess?.isRunning == true {
            completion?()
            return
        }
        guard sharedWorkspaceExists(),
              let serverURL = bundledServerURL() else {
            completion?()
            return
        }

        let process = Process()
        process.executableURL = serverURL
        process.arguments = [
            "collect",
            "--workspace",
            sharedWorkspacePath,
        ]
        process.environment = ProcessInfo.processInfo.environment

        if let logFile = openLogFile(named: "shirabe-collector.log") {
            process.standardOutput = logFile
            process.standardError = logFile
            collectorLogFile = logFile
        }

        process.terminationHandler = { [weak self, weak process] _ in
            DispatchQueue.main.async {
                guard let self, let process, self.collectorProcess === process else { return }
                self.collectorProcess = nil
                self.collectorLogFile?.closeFile()
                self.collectorLogFile = nil
                completion?()
            }
        }

        do {
            try process.run()
            collectorProcess = process
        } catch {
            collectorLogFile?.closeFile()
            collectorLogFile = nil
            completion?()
        }
    }

    private func bundledServerURL() -> URL? {
        guard let executableDirectory = Bundle.main.executableURL?.deletingLastPathComponent() else {
            return nil
        }

        let url = executableDirectory.appendingPathComponent("ShirabeServer")
        guard FileManager.default.isExecutableFile(atPath: url.path) else {
            return nil
        }
        return url
    }

    private func sharedWorkspaceExists() -> Bool {
        FileManager.default.fileExists(atPath: "\(sharedWorkspacePath)/workspace.json")
    }

    private func serverEnvironment() -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        if sharedWorkspaceExists() {
            environment["SHIRABE_DIR"] = sharedWorkspacePath
        }
        return environment
    }

    private func openServerLogFile() -> FileHandle? {
        openLogFile(named: "shirabe-server.log")
    }

    private func openLogFile(named name: String) -> FileHandle? {
        let logDirectory = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".shirabe", isDirectory: true)
        let logURL = logDirectory.appendingPathComponent(name)

        do {
            try FileManager.default.createDirectory(at: logDirectory, withIntermediateDirectories: true)
            if !FileManager.default.fileExists(atPath: logURL.path) {
                FileManager.default.createFile(atPath: logURL.path, contents: nil)
            }
            let handle = try FileHandle(forWritingTo: logURL)
            try handle.seekToEnd()
            return handle
        } catch {
            return nil
        }
    }

    private func stopBundledServer() {
        if serverProcess?.isRunning == true {
            serverProcess?.terminate()
        }
        serverProcess = nil
        serverLogFile?.closeFile()
        serverLogFile = nil
        if collectorProcess?.isRunning == true {
            collectorProcess?.terminate()
        }
        collectorProcess = nil
        collectorLogFile?.closeFile()
        collectorLogFile = nil
    }

    @MainActor
    private func checkServerAndShowContentIfNeeded() async {
        if await isServerHealthy() {
            showWebView()
        } else if !hasLoadedWebView {
            showOfflineView()
        }
    }

    private func isServerHealthy() async -> Bool {
        var request = URLRequest(url: healthURL)
        request.timeoutInterval = 1.2

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            let ok = (response as? HTTPURLResponse).map { 200..<300 ~= $0.statusCode } ?? false
            guard ok else { return false }
            if sharedWorkspaceExists() {
                return healthDatabasePath(from: data)?.hasPrefix(sharedWorkspacePath) == true
            }
            return true
        } catch {
            return false
        }
    }

    private func healthDatabasePath(from data: Data) -> String? {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return nil
        }
        return object["database"] as? String
    }

    @MainActor
    private func showWebView() {
        guard let webView, let panel else { return }
        if !hasLoadedWebView {
            webView.load(URLRequest(url: appURL))
            hasLoadedWebView = true
        }
        panel.contentView = webView
    }

    @MainActor
    private func showOfflineView() {
        guard let panel else { return }
        let offlineView = OfflineView(
            retry: { [weak self] in
                Task { await self?.ensureServerAndShowContentIfNeeded() }
            },
            openDashboard: { [weak self] in
                self?.openDashboard()
            },
            quit: {
                NSApp.terminate(nil)
            }
        )
        let hostingView = NSHostingView(rootView: offlineView)
        hostingView.frame = panel.contentView?.bounds ?? NSRect(x: 0, y: 0, width: Layout.panelWidth, height: Layout.preferredPanelHeight)
        panel.contentView = hostingView
    }

    private func showContextMenu(from button: NSStatusBarButton) {
        let menu = NSMenu()

        menu.addItem(NSMenuItem(title: "Sync Now", action: #selector(syncNow), keyEquivalent: ""))
        menu.addItem(NSMenuItem(title: "Reload", action: #selector(reloadWebView), keyEquivalent: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(NSMenuItem(title: "Open Dashboard", action: #selector(openDashboardItem), keyEquivalent: ""))
        menu.addItem(NSMenuItem.separator())
        menu.addItem(NSMenuItem(title: "Quit ShirabeBar", action: #selector(quit), keyEquivalent: "q"))

        for item in menu.items {
            item.target = self
        }

        closePanel()
        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: button.bounds.minY - 4), in: button)
    }

    @objc private func syncNow() {
        runBundledCollectorIfPossible { [weak self] in
            self?.requestServerSync {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
                    self?.reloadWebView()
                }
            }
        }
    }

    private func requestServerSync(completion: (() -> Void)? = nil) {
        var request = URLRequest(url: syncURL)
        request.httpMethod = "POST"
        request.timeoutInterval = 2

        URLSession.shared.dataTask(with: request) { _, _, _ in
            completion?()
        }.resume()
    }

    @objc private func reloadWebView() {
        if hasLoadedWebView {
            webView?.reload()
        } else {
            Task { await checkServerAndShowContentIfNeeded() }
        }
    }

    @objc private func openDashboardItem() {
        openDashboard()
    }

    private func openDashboard() {
        NSWorkspace.shared.open(dashboardURL)
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
    ) {
        guard navigationAction.targetFrame?.isMainFrame != false,
              let url = navigationAction.request.url else {
            decisionHandler(.allow)
            return
        }

        if shouldOpenExternally(url) {
            NSWorkspace.shared.open(url)
            decisionHandler(.cancel)
            return
        }

        decisionHandler(.allow)
    }

    private func shouldOpenExternally(_ url: URL) -> Bool {
        guard url.host == "127.0.0.1" || url.host == "localhost" else {
            return true
        }

        guard url.path == "/" else {
            return false
        }

        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        let isMenubar = items.contains { $0.name == "view" && $0.value == "menubar" }
        return !isMenubar
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        webView.evaluateJavaScript(
            "window.scrollTo(0,0);document.documentElement.style.overflow='hidden';document.body.style.overflow='hidden';"
        )
        hideScrollers(in: webView)
    }

    private func hideScrollers(in view: NSView) {
        if let scrollView = view as? NSScrollView {
            scrollView.hasVerticalScroller = false
            scrollView.hasHorizontalScroller = false
            scrollView.verticalScrollElasticity = .none
            scrollView.horizontalScrollElasticity = .none
            scrollView.scrollerStyle = .overlay
        }

        for subview in view.subviews {
            hideScrollers(in: subview)
        }
    }
}

private struct OfflineView: View {
    let retry: () -> Void
    let openDashboard: () -> Void
    let quit: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Shirabe")
                .font(.system(size: 28, weight: .regular, design: .monospaced))
                .foregroundStyle(Color(red: 0.9, green: 0.88, blue: 0.82))

            VStack(alignment: .leading, spacing: 10) {
                Text("Local server is offline")
                    .font(.system(size: 18, weight: .semibold, design: .monospaced))
                    .foregroundStyle(Color(red: 0.9, green: 0.88, blue: 0.82))
                Text("Retry, or check ~/.shirabe/shirabe-server.log.")
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Color(red: 0.58, green: 0.62, blue: 0.56))
            }
            .padding(18)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.black.opacity(0.18))
            .overlay(
                RoundedRectangle(cornerRadius: 4)
                    .stroke(Color(red: 0.9, green: 0.88, blue: 0.82).opacity(0.14), lineWidth: 1)
            )

            HStack {
                Button("Retry", action: retry)
                Button("Open Dashboard", action: openDashboard)
                Spacer()
                Button("Quit", action: quit)
            }
            .buttonStyle(.bordered)
        }
        .padding(20)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(
            ZStack {
                Color(red: 0.04, green: 0.06, blue: 0.06)
                GridBackground()
                LinearGradient(
                    colors: [
                        Color(red: 0.55, green: 0.71, blue: 0.42).opacity(0.13),
                        .clear
                    ],
                    startPoint: .topLeading,
                    endPoint: .bottomTrailing
                )
            }
        )
    }
}

private struct GridBackground: View {
    var body: some View {
        Canvas { context, size in
            let color = Color(red: 0.9, green: 0.88, blue: 0.82).opacity(0.035)
            var path = Path()

            stride(from: CGFloat(0), through: size.width, by: 24).forEach { x in
                path.move(to: CGPoint(x: x, y: 0))
                path.addLine(to: CGPoint(x: x, y: size.height))
            }

            stride(from: CGFloat(0), through: size.height, by: 24).forEach { y in
                path.move(to: CGPoint(x: 0, y: y))
                path.addLine(to: CGPoint(x: size.width, y: y))
            }

            context.stroke(path, with: .color(color), lineWidth: 1)
        }
    }
}
