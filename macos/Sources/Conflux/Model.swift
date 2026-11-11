import AppKit
import Foundation

enum Phase: Equatable {
    case loading
    case translocated
    case notSetUp
    case notRunning
    case waiting
    case hubUnreachable
    case problem(String)
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

    var canServe: Bool {
        switch self {
        case .ready, .modelsCooling: return true
        default: return false
        }
    }

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
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let inApplications = ["/Applications/", "\(home)/Applications/"].contains { bundle.path.hasPrefix($0) }
        if bundle.path.contains("/AppTranslocation/") || (!inApplications && !local.serviceInstalled) {
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
            if node != nil {
                phase = .hubUnreachable
            } else {
                phase = local.serviceInstalled ? .notRunning : .notSetUp
            }
        case let .answered(200, body):
            do {
                dashboard = try JSON.decoder.decode(Dashboard.self, from: body)
                phase = .ready
            } catch {
                dashboard = nil
                phase = .problem("This app can't read conflux's status. Update conflux on this Mac and on the hub.")
            }
        case .answered(409, _):
            dashboard = nil
            phase = .waiting
        case let .answered(status, body):
            dashboard = nil
            if let node, !node.hubLink.connected {
                phase = .hubUnreachable
            } else {
                let error = (try? JSONSerialization.jsonObject(with: body) as? [String: Any])?["error"] as? String
                phase = .problem(error ?? "conflux answered HTTP \(status).")
            }
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
        accounts.filter { $0.state(now: now).canServe }.count
    }

    var attention: NSColor? {
        switch phase {
        case .notRunning, .hubUnreachable, .problem:
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

    var problemSummary: String? {
        let problems = accounts.filter { $0.state(now: now).isProblem }
        if problems.isEmpty { return updateFailure == nil ? nil : "Update failed" }
        if problems.allSatisfy({ $0.needsLogin != nil }) {
            return problems.count == 1 ? "1 needs login" : "\(problems.count) need login"
        }
        return problems.count == 1 ? "1 needs attention" : "\(problems.count) need attention"
    }

    var isDown: Bool {
        phase == .notRunning || phase == .hubUnreachable
    }

    var needsAttention: Bool {
        attention != nil
    }

    func isThisMac(_ machine: Machine) -> Bool {
        node.map { machine.hostname == $0.hostname } ?? (machine.role == "hub")
    }

    var showsRouting: Bool {
        switch phase {
        case .ready, .hubUnreachable, .notRunning, .problem: return true
        default: return false
        }
    }

    var showsUpdateCard: Bool {
        updating || updateFailure != nil || latestVersion != nil
    }

    var latestVersion: String? {
        guard let latest = dashboard?.latestVersion, !outdatedMachines.isEmpty else { return nil }
        return latest
    }

    var outdatedMachines: [Machine] {
        dashboard?.machines.filter { $0.outdated && $0.online && !$0.isUpdating } ?? []
    }

    var updating: Bool {
        dashboard?.machines.contains { $0.online && $0.isUpdating } ?? false
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
        case .notRunning:
            switch local.config("role") {
            case "hub": return "This Mac is the hub"
            case "node": return local.config("hub").map { "Linked to \($0)" } ?? "Linked to a hub"
            case "waiting": return "Waiting to be linked"
            default: return runningVersion.map { "v\($0)" } ?? ""
            }
        case .notSetUp, .translocated:
            return runningVersion.map { "v\($0)" } ?? ""
        case .problem:
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
            let output = try await Command.run(self.local.cli, asHub ? ["join", "self"] : ["wait"])
            if !self.local.serviceInstalled {
                throw Command.Failure(message: output.split(separator: "\n").first.map(String.init) ?? output)
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
            request.timeoutInterval = 30
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = Data("{}".utf8)
            let (body, response) = try await URLSession.shared.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            let answer = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
            guard status == 200 else {
                throw Command.Failure(message: answer["error"] as? String ?? "The hub answered HTTP \(status).")
            }
            let started = answer["started"] as? [String] ?? []
            let skipped = answer["skipped"] as? [[String: Any]] ?? []
            if started.isEmpty, let first = skipped.first {
                let machine = first["machine"] as? String ?? "A machine"
                throw Command.Failure(message: "\(machine) was skipped: \(first["reason"] as? String ?? "no reason given")")
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
