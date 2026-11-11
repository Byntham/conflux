import SwiftUI

extension View {
    @ViewBuilder func glassCard(tint: Color, cornerRadius: CGFloat) -> some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
        if #available(macOS 26, *) {
            glassEffect(.regular.tint(tint.opacity(0.18)), in: shape)
        } else {
            background(tint.opacity(0.08), in: shape)
        }
    }

    @ViewBuilder func glassButton(prominent: Bool = false) -> some View {
        if #available(macOS 26, *) {
            if prominent { buttonStyle(.glassProminent) } else { buttonStyle(.glass) }
        } else {
            if prominent { buttonStyle(.borderedProminent) } else { buttonStyle(.bordered) }
        }
    }
}
