import AppKit
import SwiftUI

@main
struct ConfluxApp: App {
    @StateObject private var model = Model()

    var body: some Scene {
        MenuBarExtra {
            MenuView().environmentObject(model)
        } label: {
            Image(nsImage: MenuBarIcon.image(badged: model.needsAttention))
                .accessibilityLabel(model.needsAttention ? "conflux needs attention" : "conflux")
        }
        .menuBarExtraStyle(.window)
    }
}

enum MenuBarIcon {
    private static let logo: CGPath? = Bundle.main.url(forResource: "icon", withExtension: "svg")
        .flatMap { try? String(contentsOf: $0, encoding: .utf8) }
        .flatMap(Logo.path(svg:))
    private static var cache: [Bool: NSImage] = [:]

    static func image(badged: Bool) -> NSImage {
        if let image = cache[badged] { return image }
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: true) { rect in
            guard let context = NSGraphicsContext.current?.cgContext else { return false }
            context.setFillColor(NSColor.black.cgColor)
            if let logo {
                Logo.draw(logo, in: rect.insetBy(dx: 1.5, dy: 1), context: context)
            } else {
                context.fillEllipse(in: rect.insetBy(dx: 4, dy: 4))
            }
            if badged {
                let center = CGPoint(x: rect.maxX - 4, y: rect.maxY - 4)
                context.setBlendMode(.clear)
                context.fillEllipse(in: CGRect(x: center.x - 5, y: center.y - 5, width: 10, height: 10))
                context.setBlendMode(.normal)
                context.fillEllipse(in: CGRect(x: center.x - 3.25, y: center.y - 3.25, width: 6.5, height: 6.5))
            }
            return true
        }
        image.isTemplate = true
        cache[badged] = image
        return image
    }
}
