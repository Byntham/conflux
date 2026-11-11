import AppKit
import Combine
import SwiftUI

@main
enum Main {
    @MainActor static var controller: StatusController?

    @MainActor static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        controller = StatusController(model: Model())
        app.delegate = controller
        app.run()
    }
}

final class Panel: NSPanel {
    var dismiss: (() -> Void)?

    override var canBecomeKey: Bool { true }

    override func cancelOperation(_ sender: Any?) {
        dismiss?()
    }
}

@MainActor
final class StatusController: NSObject, NSWindowDelegate, NSApplicationDelegate {
    private let model: Model
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let panel: Panel
    private let host: NSHostingView<AnyView>
    private var changes: AnyCancellable?
    private var outsideClicks: Any?
    private var closedAt = Date.distantPast

    init(model: Model) {
        self.model = model
        host = NSHostingView(rootView: AnyView(MenuView().environmentObject(model)))
        panel = Panel(
            contentRect: .zero,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: true
        )
        super.init()
        panel.contentView = host
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.level = .popUpMenu
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.delegate = self
        panel.dismiss = { [weak self] in self?.close() }
        item.button?.wantsLayer = true
        item.button?.layer?.cornerRadius = 6
        item.button?.target = self
        item.button?.action = #selector(toggle)
        updateIcon()
        changes = model.objectWillChange.sink { [weak self] in
            DispatchQueue.main.async { self?.modelChanged() }
        }
        NotificationCenter.default.addObserver(
            self, selector: #selector(close), name: NSApplication.didResignActiveNotification, object: nil
        )
    }

    private func modelChanged() {
        updateIcon()
        if panel.isVisible { place() }
    }

    private func updateIcon() {
        item.button?.image = MenuBarIcon.image(badge: model.attention, dimmed: model.isDown)
        item.button?.setAccessibilityLabel(model.needsAttention ? "conflux needs attention" : "conflux")
    }

    @objc private func toggle() {
        if panel.isVisible {
            close()
        } else if Date().timeIntervalSince(closedAt) > 0.3 {
            open()
        }
    }

    private func open() {
        model.menuOpened()
        place()
        NSApp.activate(ignoringOtherApps: true)
        panel.makeKeyAndOrderFront(nil)
        item.button?.layer?.backgroundColor = NSColor.labelColor.withAlphaComponent(0.12).cgColor
        outsideClicks = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in
            Task { @MainActor in self?.close() }
        }
    }

    @objc private func close() {
        guard panel.isVisible else { return }
        panel.orderOut(nil)
        closedAt = Date()
        item.button?.layer?.backgroundColor = nil
        model.menuClosed()
        if let outsideClicks {
            NSEvent.removeMonitor(outsideClicks)
            self.outsideClicks = nil
        }
    }

    private func place() {
        guard let button = item.button, let anchor = button.window else { return }
        let size = host.fittingSize
        let below = anchor.convertToScreen(button.convert(button.bounds, to: nil))
        let screen = (anchor.screen ?? NSScreen.main)?.visibleFrame ?? .zero
        let margin = MenuView.shadowMargin
        var x = below.midX - size.width / 2
        x = min(max(x, screen.minX - margin + 8), screen.maxX - size.width + margin - 8)
        panel.setFrame(
            CGRect(x: x, y: below.minY - size.height + margin - 10, width: size.width, height: size.height),
            display: true
        )
    }

    func windowDidResignKey(_ notification: Notification) {
        close()
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !panel.isVisible { open() }
        return false
    }
}

enum MenuBarIcon {
    private static let logo: CGPath? = Bundle.main.url(forResource: "icon", withExtension: "svg")
        .flatMap { try? String(contentsOf: $0, encoding: .utf8) }
        .flatMap(Logo.path(svg:))
    private struct Key: Hashable {
        let badge: NSColor?
        let dimmed: Bool
    }

    private static var cache: [Key: NSImage] = [:]

    static func image(badge: NSColor?, dimmed: Bool) -> NSImage {
        let key = Key(badge: badge, dimmed: dimmed)
        if let image = cache[key] { return image }
        let badged = badge != nil
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: true) { rect in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            let glyph = badged ? NSColor.labelColor.withAlphaComponent(dimmed ? 0.45 : 1) : NSColor.black
            context.setFillColor(glyph.cgColor)
            if let logo {
                Logo.draw(logo, in: rect.insetBy(dx: 1.5, dy: 1), context: context)
            } else {
                context.fillEllipse(in: rect.insetBy(dx: 4, dy: 4))
            }
            if let badge {
                let dot = CGRect(x: rect.maxX - 6.5, y: rect.maxY - 7.5, width: 6.5, height: 6.5)
                context.setBlendMode(.clear)
                context.fillEllipse(in: dot.insetBy(dx: -2.5, dy: -2.5))
                context.setBlendMode(.normal)
                context.setFillColor(badge.cgColor)
                context.fillEllipse(in: dot)
            }
            return true
        }
        image.isTemplate = !badged
        image.cacheMode = .never
        cache[key] = image
        return image
    }
}
