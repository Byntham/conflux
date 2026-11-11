import AppKit
import SwiftUI

struct MenuMaterial: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = .menu
        view.blendingMode = .behindWindow
        view.state = .active
        return view
    }

    func updateNSView(_ view: NSVisualEffectView, context: Context) {}
}

extension View {
    @ViewBuilder func panelBackground(cornerRadius: CGFloat) -> some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
        if #available(macOS 26, *) {
            background(shape.fill(Palette.dim))
                .glassEffect(.regular, in: shape)
        } else {
            background(MenuMaterial().clipShape(shape))
                .overlay(shape.strokeBorder(Color.primary.opacity(0.12), lineWidth: 0.5))
        }
    }

    func glassButton(prominent: Bool = false) -> some View {
        buttonStyle(CapsuleButton(prominent: prominent))
    }
}

enum Palette {
    static let ok = dynamic(light: 0x047857, dark: 0x34D399)
    static let warn = dynamic(light: 0xA15C00, dark: 0xFBBF24)
    static let warnFill = dynamic(light: 0xD97F06, dark: 0xF59E0B)
    static let bad = dynamic(light: 0xC81E1E, dark: 0xF87171)
    static let badFill = dynamic(light: 0xC81E1E, dark: 0xE5484D)
    static let meter = dynamic(light: 0x71717A, dark: 0xD4D4D8)
    static let muted = Color.primary.opacity(0.62)
    static let dim = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.black.withAlphaComponent(0.6)
            : NSColor.white.withAlphaComponent(0.3)
    })

    private static func dynamic(light: Int, dark: Int) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let hex = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(
                srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
                green: CGFloat((hex >> 8) & 0xFF) / 255,
                blue: CGFloat(hex & 0xFF) / 255,
                alpha: 1
            )
        })
    }
}

struct CapsuleButton: ButtonStyle {
    let prominent: Bool
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        let label = configuration.label
            .font(.system(size: 13, weight: .medium))
            .lineLimit(1)
            .fixedSize()
            .foregroundStyle(prominent ? Color.white : Color.primary)
            .padding(.horizontal, 12)
            .frame(height: 26)
            .contentShape(Capsule())
            .opacity(configuration.isPressed ? 0.75 : 1)
        Group {
            if #available(macOS 26, *) {
                label.glassEffect(
                    prominent ? .regular.tint(.accentColor).interactive() : .regular.interactive(),
                    in: Capsule()
                )
            } else {
                label.background(prominent ? Color.accentColor : Color.primary.opacity(0.1), in: Capsule())
            }
        }
        .opacity(isEnabled ? 1 : 0.5)
    }
}
