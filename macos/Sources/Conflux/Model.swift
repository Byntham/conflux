import AppKit
import Foundation

enum Phase: Equatable {
    case loading
    case translocated
    case notSetUp
    case notRunning
    case waiting
    case hubUnreachable
    case ready
}

enum AccountState {
    case ready
    case needsLogin
    case billingBlocked
    case disabled
    case cooling(until: Date)
    case quotaFull
    case modelsCooling(Int)

    var isProblem: Bool {
        switch self {
        case .needsLogin, .billingBlocked: return true
        default: return false
        }
    }
}

extension Account {
    var accountCooldown: Cooldown? {
        cooldowns.first { $0.scope == "account" }
    }

    var sharedWindows: [UsageWindow] {
        usage.windows
            .filter { $0.modelScope == nil }
            .sorted { ($0.durationSecs ?? 0) < ($1.durationSecs ?? 0) }
    }

    func quotaFull(now: Date) -> Bool {
        guard !allowOverage, let updated = usage.updatedAt, now.timeIntervalSince(updated) < 30 * 60 else {
            return false
        }
        return usage.overageInUse || usage.windows.contains { window in
            window.modelScope == nil && window.usedPct >= 100 && (window.resetsAt.map { $0 > now } ?? true)
        }
    }

    func state(now: Date) -> AccountState {
        if needsLogin != nil { return .needsLogin }
        if billingBlocked != nil { return .billingBlocked }
        if !enabled { return .disabled }
        if let cooldown = accountCooldown { return .cooling(until: cooldown.until) }
        if quotaFull(now: now) { return .quotaFull }
        if !cooldowns.isEmpty { return .modelsCooling(cooldowns.count) }
        return .ready
    }

    var providerName: String {
        switch provider {
        case "claude": return "Claude"
        case "codex": return "Codex"
        default: return provider.capitalized
        }
    }
}

@MainActor
final class Model: ObservableObject {
    @Published private(set) var phase: Phase = .loading
    @Published private(set) var dashboard: Dashboard?
    @Published private(set) var node: NodeInfo?
    @Published private(set) var routing: Bool?
    @Published private(set) var busy: String?
    @Published var failure: String?
    @Published private(set) var now = Date()

    let bundle = Bundle.main.bundleURL
    let local: Local
    let runningVersion = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
    private let session: URLSession
    private var menuOpen = false

    init() {
        local = Local.fromEnvironment(bundle: bundle)
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 5
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: config)
        Task { await self.poll() }
    }

    private func poll() async {
        while true {
            await refresh()
            let seconds: UInt64 = menuOpen || busy != nil || updating ? 3 : 20
            try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        }
    }

    func menuOpened() {
        menuOpen = true
        Task { await refresh() }
    }

    func menuClosed() {
        menuOpen = false
    }

    var dashboardURL: URL {
        local.base(port: local.port)
    }

    func refresh() async {
        now = Date()
        relaunchIfReplaced()
        if bundle.path.contains("/AppTranslocation/") {
            phase = .translocated
            return
        }
        routing = local.routing
        let base = local.base(port: local.port)
        let nodeAnswer = await fetch(base.appendingPathComponent("conflux/node"))
        if case let .answered(200, body) = nodeAnswer {
            node = try? JSON.decoder.decode(NodeInfo.self, from: body)
        } else {
            node = nil
        }
        switch await fetch(base.appendingPathComponent("conflux/api/dashboard")) {
        case .unreachable:
            dashboard = nil
            phase = local.serviceInstalled ? .notRunning : .notSetUp
        case let .answered(200, body):
            do {
                dashboard = try JSON.decoder.decode(Dashboard.self, from: body)
                phase = .ready
            } catch {
                dashboard = nil
                phase = .hubUnreachable
            }
        case .answered(409, _):
            dashboard = nil
            phase = .waiting
        case .answered:
            phase = node == nil ? .notRunning : .hubUnreachable
        }
    }

    private func fetch(_ url: URL) async -> Fetched {
        do {
            let (body, response) = try await session.data(from: url)
            return .answered(status: (response as? HTTPURLResponse)?.statusCode ?? 0, body: body)
        } catch {
            return .unreachable
        }
    }

    private func relaunchIfReplaced() {
        let plist = bundle.appendingPathComponent("Contents/Info.plist")
        guard let info = NSDictionary(contentsOf: plist),
              let onDisk = info["CFBundleShortVersionString"] as? String,
              let runningVersion, onDisk != runningVersion
        else { return }
        let relaunch = Process()
        relaunch.executableURL = URL(fileURLWithPath: "/bin/sh")
        relaunch.arguments = ["-c", "sleep 1; /usr/bin/open \"$0\"", bundle.path]
        try? relaunch.run()
        NSApp.terminate(nil)
    }

    var accounts: [Account] {
        dashboard?.accounts ?? []
    }

    var readyCount: Int {
        accounts.filter { if case .ready = $0.state(now: now) { return true } else { return false } }.count
    }

    var attention: NSColor? {
        switch phase {
        case .notRunning, .hubUnreachable:
            return .systemRed
        case .ready:
            if accounts.contains(where: { $0.state(now: now).isProblem }) || updateFailure != nil {
                return .systemRed
            }
            let enabled = accounts.filter(\.enabled)
            return !enabled.isEmpty && readyCount == 0 ? .systemOrange : nil
        default:
            return nil
        }
    }

    var needsAttention: Bool {
        attention != nil
    }

    func isThisMac(_ machine: Machine) -> Bool {
        node.map { machine.hostname == $0.hostname } ?? (machine.role == "hub")
    }

    var showsRouting: Bool {
        [.ready, .hubUnreachable, .notRunning].contains(phase)
    }

    var latestVersion: String? {
        guard let dashboard, let latest = dashboard.latestVersion else { return nil }
        let behind = dashboard.machines.contains(where: \.outdated)
        return behind ? latest : nil
    }

    var outdatedMachines: [Machine] {
        dashboard?.machines.filter(\.outdated) ?? []
    }

    var updating: Bool {
        dashboard?.machines.contains { ($0.update?.state).map { $0 != "failed" } ?? false } ?? false
    }

    var updateFailure: String? {
        guard let machine = dashboard?.machines.first(where: { $0.update?.state == "failed" }) else { return nil }
        return "\(machine.hostname): \(machine.update?.error ?? "update failed")"
    }

    var roleLine: String {
        switch phase {
        case .ready:
            let version = latestVersion == nil ? dashboard.map { " · v\($0.hub.version)" } ?? "" : ""
            if let node { return "Linked to \(node.hub)\(version)" }
            return "This Mac is the hub\(version)"
        case .hubUnreachable:
            return node.map { "Linked to \($0.hub)" } ?? "Linked to a hub"
        case .waiting:
            return "Waiting to be linked"
        case .notRunning, .notSetUp, .translocated:
            return runningVersion.map { "v\($0)" } ?? ""
        case .loading:
            return "Connecting…"
        }
    }

    private func perform(_ label: String, _ work: @escaping () async throws -> Void) {
        guard busy == nil else { return }
        busy = label
        failure = nil
        Task {
            do {
                try await work()
            } catch {
                failure = error.localizedDescription
            }
            busy = nil
            await refresh()
        }
    }

    private func conflux(_ arguments: String...) async throws {
        _ = try await Command.run(local.cli, arguments)
    }

    private func launchctl(_ arguments: String...) async throws {
        _ = try await Command.run(URL(fileURLWithPath: "/bin/launchctl"), arguments)
    }

    func setRouting(_ on: Bool) {
        perform(on ? "Turning on…" : "Turning off…") { try await self.conflux(on ? "on" : "off") }
    }

    func setUp(asHub: Bool) {
        perform("Setting up…") {
            _ = try? await Command.run(
                URL(fileURLWithPath: "/usr/bin/xattr"),
                ["-dr", "com.apple.quarantine", self.bundle.path]
            )
            if asHub {
                try await self.conflux("join", "self")
            } else {
                try await self.conflux("wait")
            }
        }
    }

    func restart() {
        perform("Restarting…") { try await self.launchctl("kickstart", "-k", self.local.serviceTarget) }
    }

    func start() {
        perform("Starting…") {
            do {
                try await self.launchctl("kickstart", "-k", self.local.serviceTarget)
            } catch {
                try await self.launchctl("bootstrap", "gui/\(getuid())", self.local.servicePlist.path)
            }
        }
    }

    func updateAll() {
        perform("Starting update…") {
            var request = URLRequest(url: self.local.base(port: self.local.port).appendingPathComponent("conflux/api/update"))
            request.httpMethod = "POST"
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = Data("{}".utf8)
            let (body, response) = try await self.session.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard status == 200 else {
                let message = (try? JSONSerialization.jsonObject(with: body) as? [String: Any])?["error"] as? String
                throw Command.Failure(message: message ?? "The hub answered HTTP \(status).")
            }
        }
    }

    func openDashboard() {
        NSWorkspace.shared.open(dashboardURL)
    }

    func revealInFinder() {
        NSWorkspace.shared.activateFileViewerSelecting([bundle])
    }
}
