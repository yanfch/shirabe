import AppKit
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
        static let preferredPanelHeight: CGFloat = 810
        static let screenInset: CGFloat = 8
        static let panelGap: CGFloat = 8
        static let cornerRadius: CGFloat = 18
    }

    private static var retainedDelegate: AppDelegate?

    private let appURL = URL(string: "http://127.0.0.1:7778/?view=menubar")!
    private let dashboardURL = URL(string: "http://127.0.0.1:7778/?page=usage")!
    private let healthURL = URL(string: "http://127.0.0.1:7778/api/health")!
    private let syncURL = URL(string: "http://127.0.0.1:7778/api/sync")!

    private var statusItem: NSStatusItem?
    private var panel: NSPanel?
    private var webView: WKWebView?
    private var outsideClickMonitor: Any?
    private var hasLoadedWebView = false

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
        Task { await checkServerAndShowContentIfNeeded() }
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let outsideClickMonitor {
            NSEvent.removeMonitor(outsideClickMonitor)
        }
    }

    private func configureStatusItem() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem = item

        guard let button = item.button else { return }
        button.image = NSImage(systemSymbolName: "chart.bar.xaxis", accessibilityDescription: "Shirabe")
        button.image?.isTemplate = true
        button.target = self
        button.action = #selector(handleStatusItemClick(_:))
        button.sendAction(on: [.leftMouseUp, .rightMouseUp])
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
            Task { await checkServerAndShowContentIfNeeded() }
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
            let (_, response) = try await URLSession.shared.data(for: request)
            return (response as? HTTPURLResponse).map { 200..<300 ~= $0.statusCode } ?? false
        } catch {
            return false
        }
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
                Task { await self?.checkServerAndShowContentIfNeeded() }
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
        var request = URLRequest(url: syncURL)
        request.httpMethod = "POST"
        request.timeoutInterval = 2

        URLSession.shared.dataTask(with: request) { [weak self] _, _, _ in
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) {
                self?.reloadWebView()
            }
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
                Text("Start Shirabe with `just restart`, then retry.")
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
