import AppKit
import SwiftUI

struct MenuView: View {
    static let shadowMargin: CGFloat = 24
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Header()
            Separator()
            content
            if let failure = model.failure {
                Text(failure)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.bad)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 14)
                    .padding(.top, 6)
            }
            Separator()
            Actions()
        }
        .padding(.top, 10)
        .padding(.bottom, 5)
        .frame(width: 340)
        .fixedSize(horizontal: false, vertical: true)
        .panelBackground(cornerRadius: 16)
        .shadow(color: .black.opacity(0.22), radius: 14, y: 8)
        .padding(Self.shadowMargin)
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .loading:
            Notice(title: "Connecting to conflux…", detail: nil)
        case .translocated:
            Notice(
                title: "Move Conflux to Applications",
                detail: "macOS is running Conflux from a temporary copy. Drag it into your Applications folder, then open it again."
            ) {
                Button("Show in Finder") { model.revealInFinder() }.glassButton(prominent: true)
            }
        case .notSetUp:
            Notice(
                title: "conflux isn't set up on this Mac",
                detail: "Link this Mac to the hub on another machine, or make this Mac the hub that holds your accounts."
            ) {
                Button("Use another hub") { model.setUp(asHub: false) }.glassButton(prominent: true)
                Button("Make this the hub") { model.setUp(asHub: true) }.glassButton()
            }
        case .notRunning:
            Notice(
                title: "conflux isn't running",
                detail: model.routing == true
                    ? "Claude Code and Codex on this Mac can't connect until it starts. To use their own logins instead, turn off routing below."
                    : "Claude Code and Codex can't reach it until it starts."
            ) {
                Button("Start conflux") { model.start() }.glassButton(prominent: true)
            }
        case .waiting:
            Notice(
                title: "Waiting to be linked",
                detail: "On your hub's dashboard, open Machines and click Find machines."
            ) {
                Button("Make this the hub") { model.setUp(asHub: true) }.glassButton()
            }
        case .hubUnreachable:
            Notice(
                title: "Can't reach the hub",
                detail: "\(model.node?.hub ?? "The hub") isn't answering. To keep working, turn off routing below, then restart Claude Code or Codex."
            )
        case .ready:
            Accounts()
            UpdateBanner()
        }
    }
}

struct Header: View {
    @EnvironmentObject var model: Model

    var body: some View {
        HStack(spacing: 10) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 30, height: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text("conflux").font(.system(size: 13, weight: .semibold))
                Text(model.roleLine)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 8)
            if let busy = model.busy {
                ProgressView().controlSize(.small)
                Text(busy).font(.system(size: 11)).foregroundStyle(Palette.muted)
            } else {
                StatusDot()
            }
        }
        .padding(.horizontal, 14)
    }
}

struct StatusDot: View {
    @EnvironmentObject var model: Model

    var body: some View {
        let (color, label) = summary
        HStack(spacing: 5) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text(label).font(.system(size: 11)).foregroundStyle(Palette.muted)
        }
    }

    private var summary: (Color, String) {
        switch model.phase {
        case .ready:
            if model.accounts.isEmpty { return (Palette.meter, "No accounts") }
            let ready = "\(model.readyCount) of \(model.accounts.count) ready"
            return (model.needsAttention ? Palette.warnFill : Palette.ok, ready)
        case .notRunning, .hubUnreachable:
            return (Palette.badFill, "Offline")
        case .waiting:
            return (Palette.warnFill, "Waiting")
        default:
            return (.clear, "")
        }
    }
}

struct Notice<Buttons: View>: View {
    let title: String
    let detail: String?
    @ViewBuilder var buttons: Buttons

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.system(size: 13, weight: .semibold))
            if let detail {
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) { buttons }
                .padding(.top, 4)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 4)
    }
}

extension Notice where Buttons == EmptyView {
    init(title: String, detail: String?) {
        self.init(title: title, detail: detail) { EmptyView() }
    }
}

struct Accounts: View {
    @EnvironmentObject var model: Model

    var body: some View {
        if model.accounts.isEmpty {
            Notice(
                title: "No accounts yet",
                detail: "Add a Claude or ChatGPT account from the dashboard."
            ) {
                Button("Add account…") { model.openDashboard() }.glassButton(prominent: true)
            }
        } else {
            let rows = VStack(alignment: .leading, spacing: 12) {
                HStack {
                    Text("Accounts")
                    Spacer()
                    Text("Resets in").frame(width: 52, alignment: .trailing)
                }
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(Palette.muted)
                .padding(.bottom, -6)
                ForEach(model.accounts) { account in
                    AccountRow(account: account, now: model.now, openDashboard: model.openDashboard)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 4)
            if model.accounts.count > 4 {
                ScrollView { rows }.frame(height: 330)
            } else {
                rows
            }
        }
    }
}

struct AccountRow: View {
    let account: Account
    let now: Date
    let openDashboard: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                Text(account.email ?? account.id)
                    .font(.system(size: 13, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(kind)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
                    .lineLimit(1)
                Spacer(minLength: 6)
                StatePill(state: account.state(now: now), now: now)
            }
            let windows = account.sharedWindows
            if account.needsLogin != nil {
                Button("Log in again in the dashboard", action: openDashboard)
                    .buttonStyle(.link)
                    .font(.system(size: 11))
            } else if windows.isEmpty {
                Text("No usage data yet")
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
            } else {
                ForEach(windows) { window in
                    Meter(window: window, now: now)
                }
            }
        }
        .opacity(account.enabled ? 1 : 0.55)
    }

    private var kind: String {
        [account.providerName, account.plan.map { $0.replacingOccurrences(of: "_", with: " ").capitalized }]
            .compactMap { $0 }
            .joined(separator: " ")
    }
}

struct StatePill: View {
    let state: AccountState
    let now: Date

    var body: some View {
        switch state {
        case .ready:
            HStack(spacing: 5) {
                Circle().fill(Palette.ok).frame(width: 6, height: 6)
                Text("Ready").font(.system(size: 11)).foregroundStyle(Palette.muted)
            }
        case .disabled:
            Text("Disabled").font(.system(size: 11)).foregroundStyle(Palette.muted)
        default:
            let (text, color, tint) = problem
            Text(text)
                .font(.system(size: 10.5, weight: .semibold))
                .foregroundStyle(color)
                .padding(.horizontal, 7)
                .padding(.vertical, 2)
                .background(tint.opacity(0.2), in: Capsule())
                .lineLimit(1)
                .fixedSize()
        }
    }

    private var problem: (String, Color, Color) {
        switch state {
        case .needsLogin: return ("Needs login", Palette.bad, Palette.badFill)
        case .billingBlocked: return ("Billing blocked", Palette.bad, Palette.badFill)
        case let .cooling(until): return ("Cooling \(Countdown.text(until, now: now))", Palette.warn, Palette.warnFill)
        case .quotaFull: return ("Quota full", Palette.warn, Palette.warnFill)
        case let .modelsCooling(n): return ("\(n) \(n == 1 ? "model" : "models") cooling", Palette.warn, Palette.warnFill)
        case .ready, .disabled: return ("", .clear, .clear)
        }
    }
}

struct Meter: View {
    let window: UsageWindow
    let now: Date

    var body: some View {
        let pct = min(100, max(0, window.usedPct.rounded()))
        HStack(spacing: 8) {
            Text(name)
                .font(.system(size: 11))
                .foregroundStyle(Palette.muted)
                .frame(width: 42, alignment: .leading)
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.14))
                    Capsule()
                        .fill(color(pct))
                        .frame(width: max(pct > 0 ? 4 : 0, geometry.size.width * pct / 100))
                }
            }
            .frame(height: 6)
            Text("\(Int(pct))%")
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(pct >= 100 ? Palette.bad : pct >= 80 ? Palette.warn : .primary)
                .frame(width: 34, alignment: .trailing)
            Text(window.resetsAt.map { Countdown.text($0, now: now) } ?? "")
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(Palette.muted)
                .frame(width: 52, alignment: .trailing)
                .lineLimit(1)
        }
        .help(window.resetsAt.map { "\(name) limit resets in \(Countdown.text($0, now: now))" } ?? name)
    }

    private func color(_ pct: Double) -> Color {
        pct >= 100 ? Palette.badFill : pct >= 80 ? Palette.warnFill : Palette.meter
    }

    private var name: String {
        switch window.key {
        case "five_hour": return "5 hour"
        case "seven_day": return "7 day"
        default:
            guard let seconds = window.durationSecs else { return window.key }
            let hours = Double(seconds) / 3600
            return hours >= 48 ? "\(Int((hours / 24).rounded())) day" : "\(Int(hours.rounded())) hour"
        }
    }
}

enum Countdown {
    static func text(_ date: Date, now: Date) -> String {
        var seconds = Int(date.timeIntervalSince(now).rounded())
        if seconds <= 0 { return "now" }
        let days = seconds / 86400
        seconds -= days * 86400
        let hours = seconds / 3600
        seconds -= hours * 3600
        let minutes = seconds / 60
        if days > 0 { return hours > 0 ? "\(days)d \(hours)h" : "\(days)d" }
        if hours > 0 { return minutes > 0 ? "\(hours)h \(minutes)m" : "\(hours)h" }
        return "\(max(1, minutes))m"
    }
}

struct UpdateBanner: View {
    @EnvironmentObject var model: Model

    var body: some View {
        if model.updating {
            banner(title: "Updating…", detail: "Machines switch over as they finish their current requests.") {
                ProgressView().controlSize(.small)
            }
        } else if let failure = model.updateFailure {
            banner(title: "An update failed", detail: failure) {
                Button("Try again") { model.updateAll() }.glassButton(compact: true)
            }
        } else if let latest = model.latestVersion {
            let behind = model.outdatedMachines
            banner(
                title: "conflux \(latest) is available",
                detail: behind.count == 1
                    ? "\(behind[0].hostname) is on v\(behind[0].version)."
                    : "\(behind.count) machines are behind."
            ) {
                Button(behind.count == 1 ? "Update" : "Update all") { model.updateAll() }
                    .glassButton(prominent: true, compact: true)
            }
        }
    }

    private func banner<Trailing: View>(title: String, detail: String, @ViewBuilder trailing: () -> Trailing) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(.primary)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 13, weight: .semibold))
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            trailing()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(Color.accentColor.opacity(0.12), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .padding(.horizontal, 10)
        .padding(.top, 10)
    }
}

struct Actions: View {
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let routing = model.routing, model.showsRouting {
                HStack {
                    Text("Route this Mac through conflux").font(.system(size: 13))
                    Spacer()
                    Toggle("", isOn: Binding(get: { routing }, set: { model.setRouting($0) }))
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                }
                .padding(.horizontal, 9)
                .frame(height: 26)
                .help("Off makes Claude Code and Codex on this Mac use their own logins.")
                Separator(inset: 9)
            }
            if model.phase == .ready || model.phase == .hubUnreachable {
                MenuRow(title: "Open dashboard", action: model.openDashboard)
                MenuRow(title: "Add account…", action: model.openDashboard)
                Separator(inset: 9)
            }
            if model.local.serviceInstalled && model.phase != .notRunning {
                MenuRow(title: "Restart conflux", action: model.restart)
            }
            MenuRow(title: "Quit menu bar app", action: { NSApp.terminate(nil) }) {
                Text("⌘Q").font(.system(size: 13)).foregroundStyle(HierarchicalShapeStyle.secondary)
            }
            .keyboardShortcut("q")
            .help("conflux keeps running in the background.")
        }
        .disabled(model.busy != nil)
        .padding(.horizontal, 5)
    }
}

struct Separator: View {
    var inset: CGFloat = 14

    var body: some View {
        Divider()
            .padding(.horizontal, inset)
            .padding(.vertical, 6)
    }
}

struct MenuRow<Trailing: View>: View {
    let title: String
    let action: () -> Void
    @ViewBuilder var trailing: Trailing
    @State private var hovered = false
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack {
                Text(title).font(.system(size: 13))
                Spacer()
                trailing
            }
            .foregroundStyle(hovered && isEnabled ? Color.white : Color.primary)
            .padding(.horizontal, 9)
            .frame(height: 24)
            .contentShape(Rectangle())
            .background(
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(hovered && isEnabled ? Color.accentColor : .clear)
            )
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
    }
}

extension MenuRow where Trailing == EmptyView {
    init(title: String, action: @escaping () -> Void) {
        self.init(title: title, action: action) { EmptyView() }
    }
}
