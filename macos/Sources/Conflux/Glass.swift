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
            background(shape.fill(Palette.glassTint))
                .glassEffect(.regular, in: shape)
                .overlay(shape.strokeBorder(Palette.rim, lineWidth: 0.5))
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
    static let bad = dynamic(light: 0xC81E1E, dark: 0xFCA5A5)
    static let badFill = dynamic(light: 0xC81E1E, dark: 0xE5484D)
    static let link = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0.6, green: 0.8, blue: 1, alpha: 1)
            : NSColor.linkColor
    })
    static let pillBase = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.black.withAlphaComponent(0.3)
            : NSColor.clear
    })
    static let meter = dynamic(light: 0x71717A, dark: 0xA1A1AA)
    static let muted = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.white.withAlphaComponent(0.62)
            : NSColor.black.withAlphaComponent(0.7)
    })
    static let track = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.white.withAlphaComponent(0.22)
            : NSColor.black.withAlphaComponent(0.18)
    })
    static let rim = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.white.withAlphaComponent(0.14)
            : NSColor.clear
    })
    static let glassTint = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor.white.withAlphaComponent(0.04)
            : NSColor.white.withAlphaComponent(0.18)
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

    func makeBody(configuration: Configuration) -> some View {
        CapsuleButtonBody(configuration: configuration, prominent: prominent)
    }
}

private struct CapsuleButtonBody: View {
    let configuration: ButtonStyleConfiguration
    let prominent: Bool
    @State private var hovered = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        let pressed = configuration.isPressed && isEnabled
        let lit = hovered && isEnabled
        let label = configuration.label
            .font(.system(size: 13, weight: .medium))
            .lineLimit(1)
            .fixedSize()
            .foregroundStyle(prominent ? Color.white : Color.primary)
            .padding(.horizontal, 12)
            .frame(height: 26)
            .background {
                Capsule()
                    .fill(prominent ? Color.white : Color.primary)
                    .opacity(lit && !pressed ? (prominent ? 0.16 : 0.09) : 0)
                Capsule().fill(Color.black).opacity(pressed ? 0.2 : 0)
            }
            .contentShape(Capsule())
        Group {
            if #available(macOS 26, *) {
                label.glassEffect(prominent ? .regular.tint(.accentColor) : .regular, in: Capsule())
            } else {
                label.background(prominent ? Color.accentColor : Color.primary.opacity(0.1), in: Capsule())
            }
        }
        .scaleEffect(pressed ? 0.96 : 1)
        .opacity(isEnabled ? 1 : 0.5)
        .animation(.easeOut(duration: 0.15), value: lit)
        .animation(.spring(response: 0.2, dampingFraction: 0.7), value: pressed)
        .onHover { hovered = $0 }
    }
}

struct LinkButton: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        LinkButtonBody(configuration: configuration)
    }
}

private struct LinkButtonBody: View {
    let configuration: ButtonStyleConfiguration
    @State private var hovered = false

    var body: some View {
        configuration.label
            .foregroundStyle(Palette.link)
            .underline(hovered)
            .opacity(configuration.isPressed ? 0.6 : 1)
            .contentShape(Rectangle())
            .onHover { hovered = $0 }
    }
}
