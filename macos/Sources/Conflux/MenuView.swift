import AppKit
import SwiftUI

struct MenuView: View {
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Header()
            Divider().padding(.vertical, 6)
            content
            if let failure = model.failure {
                Text(failure)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 14)
                    .padding(.top, 6)
            }
            Divider().padding(.vertical, 6)
            Actions()
        }
        .padding(.vertical, 8)
        .frame(width: 340)
        .onAppear { model.menuOpened() }
        .onDisappear { model.menuClosed() }
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
                Button("Show in Finder") { model.revealInFinder() }
            }
        case .notSetUp:
            Notice(
                title: "conflux isn't set up on this Mac",
                detail: "Make this Mac the hub that holds your accounts, or wait for your hub to link it."
            ) {
                Button("Make this Mac the hub") { model.setUp(asHub: true) }
                Button("Wait to be linked") { model.setUp(asHub: false) }
            }
        case .notRunning:
            Notice(title: "conflux isn't running", detail: "Claude Code and Codex can't reach it until it starts.") {
                Button("Start conflux") { model.start() }
            }
        case .waiting:
            Notice(
                title: "Waiting to be linked",
                detail: "On your hub's dashboard, open Machines and click Find machines."
            ) {
                Button("Make this Mac the hub") { model.setUp(asHub: true) }
            }
        case .hubUnreachable:
            Notice(
                title: "Can't reach the hub",
                detail: "\(model.node?.hub ?? "The hub") isn't answering. To keep working, turn off routing below and restart Claude Code or Codex."
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
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 8)
            if let busy = model.busy {
                ProgressView().controlSize(.small)
                Text(busy).font(.system(size: 11)).foregroundStyle(.secondary)
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
            Text(label).font(.system(size: 11)).foregroundStyle(.secondary)
        }
    }

    private var summary: (Color, String) {
        switch model.phase {
        case .ready:
            if model.accounts.isEmpty { return (.secondary, "No accounts") }
            if model.needsAttention { return (.orange, "\(model.readyCount) of \(model.accounts.count) ready") }
            return (.green, "\(model.readyCount) of \(model.accounts.count) ready")
        case .notRunning, .hubUnreachable:
            return (.red, "Offline")
        case .waiting:
            return (.orange, "Waiting")
        default:
            return (.secondary, "")
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
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) { buttons }
                .controlSize(.regular)
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
                Button("Add account…") { model.openDashboard() }
            }
        } else {
            let rows = VStack(alignment: .leading, spacing: 12) {
                ForEach(model.accounts) { account in
                    AccountRow(account: account, now: model.now)
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

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                Text(account.email ?? account.id)
                    .font(.system(size: 12.5, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(kind)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer(minLength: 6)
                StatePill(state: account.state(now: now), now: now)
            }
            let windows = account.sharedWindows
            if windows.isEmpty {
                Text("No usage data yet")
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
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
        let (text, color) = look
        Text(text)
            .font(.system(size: 10.5, weight: .medium))
            .foregroundStyle(color)
            .padding(.horizontal, 6)
            .padding(.vertical, 1.5)
            .background(color.opacity(0.13), in: Capsule())
            .lineLimit(1)
            .fixedSize()
    }

    private var look: (String, Color) {
        switch state {
        case .ready: return ("Ready", .green)
        case .needsLogin: return ("Needs login", .red)
        case .billingBlocked: return ("Billing blocked", .red)
        case .disabled: return ("Disabled", .secondary)
        case let .cooling(until): return ("Cooling \(Countdown.text(until, now: now))", .orange)
        case .quotaFull: return ("Quota full", .orange)
        case let .modelsCooling(n): return ("\(n) \(n == 1 ? "model" : "models") cooling", .orange)
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
                .foregroundStyle(.secondary)
                .frame(width: 42, alignment: .leading)
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.1))
                    Capsule()
                        .fill(color(pct))
                        .frame(width: max(pct > 0 ? 4 : 0, geometry.size.width * pct / 100))
                }
            }
            .frame(height: 5)
            Text("\(Int(pct))%")
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(pct >= 80 ? color(pct) : .primary)
                .frame(width: 34, alignment: .trailing)
            Text(window.resetsAt.map { "resets \(Countdown.text($0, now: now))" } ?? "")
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 88, alignment: .leading)
                .lineLimit(1)
        }
    }

    private func color(_ pct: Double) -> Color {
        pct >= 100 ? .red : pct >= 80 ? .orange : .accentColor
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
                Button("Try again") { model.updateAll() }
            }
        } else if let latest = model.latestVersion {
            let count = model.outdatedCount
            banner(
                title: "conflux \(latest) is available",
                detail: count == 1 ? "1 machine is behind." : "\(count) machines are behind."
            ) {
                Button("Update") { model.updateAll() }
                    .buttonStyle(.borderedProminent)
            }
        }
    }

    private func banner<Trailing: View>(title: String, detail: String, @ViewBuilder trailing: () -> Trailing) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .foregroundStyle(Color.accentColor)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 12, weight: .semibold))
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            trailing().controlSize(.small)
        }
        .padding(10)
        .background(Color.accentColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .padding(.horizontal, 10)
        .padding(.top, 10)
    }
}

struct Actions: View {
    @EnvironmentObject var model: Model

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let routing = model.routing {
                MenuRow(title: "Route Claude Code and Codex here", action: { model.setRouting(!routing) }) {
                    Toggle("", isOn: Binding(get: { routing }, set: { model.setRouting($0) }))
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                }
                .help("Off makes Claude Code and Codex on this Mac use their own logins.")
            }
            if model.phase == .ready || model.phase == .hubUnreachable {
                MenuRow(title: "Open dashboard", action: model.openDashboard)
                MenuRow(title: "Add account…", action: model.openDashboard)
            }
            if model.local.serviceInstalled && model.phase != .notRunning {
                MenuRow(title: "Restart conflux", action: model.restart)
            }
            Divider().padding(.vertical, 6)
            MenuRow(title: "Quit menu bar app", action: { NSApp.terminate(nil) }) {
                Text("⌘Q").font(.system(size: 12)).foregroundStyle(.secondary)
            }
            .keyboardShortcut("q")
            .help("conflux keeps running in the background.")
        }
        .disabled(model.busy != nil)
        .padding(.horizontal, 5)
    }
}

struct MenuRow<Trailing: View>: View {
    let title: String
    let action: () -> Void
    @ViewBuilder var trailing: Trailing
    @State private var hovered = false

    var body: some View {
        Button(action: action) {
            HStack {
                Text(title).font(.system(size: 13))
                Spacer()
                trailing
            }
            .padding(.horizontal, 9)
            .frame(height: 24)
            .contentShape(Rectangle())
            .background(
                RoundedRectangle(cornerRadius: 5).fill(hovered ? Color.primary.opacity(0.1) : .clear)
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
