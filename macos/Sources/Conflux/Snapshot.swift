import AppKit
import SwiftUI

enum Snapshot {
    @MainActor static func run(to file: URL) -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.prohibited)
        Task {
            do {
                try await write(to: file)
                exit(0)
            } catch {
                FileHandle.standardError.write(Data("snapshot failed: \(error.localizedDescription)\n".utf8))
                exit(1)
            }
        }
        app.run()
        exit(1)
    }

    @MainActor private static func write(to file: URL) async throws {
        let model = Model()
        model.menuOpened()
        await model.refresh()
        let sides = [NSAppearance.Name.aqua, .darkAqua].map { name in
            let menu = MenuView()
                .environmentObject(model)
                .background(Color(nsColor: .windowBackgroundColor))
            let side = NSHostingView(rootView: menu)
            side.appearance = NSAppearance(named: name)
            return side
        }
        let both = NSStackView(views: sides)
        both.spacing = 0
        let offScreen = CGPoint(x: -20000, y: -20000)
        let window = NSWindow(
            contentRect: CGRect(origin: offScreen, size: both.fittingSize),
            styleMask: .borderless,
            backing: .buffered,
            defer: false
        )
        window.contentView = both
        window.animationBehavior = .none
        window.orderFrontRegardless()
        window.displayIfNeeded()
        CATransaction.flush()
        guard let image = CGWindowListCreateImage(
            .null, .optionIncludingWindow, CGWindowID(window.windowNumber), [.boundsIgnoreFraming, .bestResolution]
        ), let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
        else {
            throw Command.Failure(message: "could not draw the menu")
        }
        try png.write(to: file)
    }
}
