import AppKit
import Foundation

enum Phase: Equatable {
    case loading
    case outsideApplications
    case notSetUp
    case notRunning
    case waiting
    case notApproved
    case hubUnreachable
    case problem(String)
    case ready
}

enum AccountState: Equatable {
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

    var coolingUntil: Date? {
        if case let .cooling(until) = self { return until }
        return nil
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
}

struct Pool: Identifiable {
    let provider: String
    let accounts: [Account]

    var id: String { provider }

    var name: String {
        switch provider {
        case "claude": return "Claude"
        case "codex": return "Codex"
        default: return provider.capitalized
        }
    }

    func windows(now: Date) -> [PoolWindow] {
        let members = accounts.filter {
            $0.enabled && $0.needsLogin == nil && $0.billingBlocked == nil && !$0.sharedWindows.isEmpty
        }
        let shared = members.flatMap(\.sharedWindows).sorted { ($0.durationSecs ?? .max) < ($1.durationSecs ?? .max) }
        let bucket = { (window: UsageWindow) in
            window.durationSecs.map { $0 >= 2 * 86400 ? "long" : "short" } ?? window.key
        }
        var buckets: [String] = []
        for window in shared where !buckets.contains(bucket(window)) { buckets.append(bucket(window)) }
        return buckets.map { name in
            let group = shared.filter { bucket($0) == name }
            let used = group.map { window in
                window.resetsAt.map { $0 <= now } == true ? 0 : min(100, max(0, window.usedPct))
            }
            let refills = Dictionary(
                zip(group, used).compactMap { window, amount -> (Date, Double)? in
                    amount > 0 ? window.resetsAt.map { ($0, amount) } : nil
                },
                uniquingKeysWith: +
            )
            let refill = refills.sorted { $0.key < $1.key }
                .map { ($0.key, $0.value / Double(members.count)) }
                .first { $0.1 >= 0.5 }
            return PoolWindow(
                window: UsageWindow(
                    key: name,
                    usedPct: used.reduce(0, +) / Double(members.count),
                    resetsAt: refill?.0,
                    durationSecs: group[0].durationSecs,
                    modelScope: nil
                ),
                refillPct: refill?.1 ?? 0
            )
        }
    }
}

struct PoolWindow: Identifiable {
    let window: UsageWindow
    let refillPct: Double

    var id: String { window.key }
}

@MainActor
final class Model: ObservableObject {
    @Published private(set) var phase: Phase = .loading
    @Published private(set) var dashboard: Dashboard?
    @Published private(set) var waiting: WaitingStatus?
    @Published private(set) var hubs: [FoundHub]?
    @Published private(set) var node: NodeInfo?
    @Published private(set) var routing: Bool?
    @Published private(set) var busy: String?
    @Published var failure: String?
    @Published private(set) var checkedForUpdates = false
    @Published private(set) var now = Date()

    let bundle = Bundle.main.bundleURL
    let local: Local
    let runningVersion = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
    private let session: URLSession
    private var menuOpen = false
    private var seen: [URL: (tag: String, body: Data)] = [:]

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
            let seconds: UInt64 = menuOpen || busy != nil || updating || unsettled ? 3 : 20
            try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
        }
    }

    func menuOpened() {
        menuOpen = true
        Task { await refresh() }
    }

    func menuClosed() {
        menuOpen = false
        checkedForUpdates = false
    }

    var dashboardURL: URL {
        local.base(port: local.port)
    }

    func refresh() async {
        now = Date()
        var waitingNow: WaitingStatus?
        defer {
            waiting = waitingNow
            if phase != .waiting { hubs = nil }
        }
        relaunchIfReplaced()
        if !local.devCopy && !inApplications && !local.serviceInstalled {
            phase = .outsideApplications
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
        if node?.notApproved == true {
            dashboard = nil
            phase = .notApproved
            return
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
            if case let .answered(200, body) = await fetch(base.appendingPathComponent("conflux/api/status")) {
                waitingNow = try? JSON.decoder.decode(WaitingStatus.self, from: body)
            }
            if menuOpen {
                let found = await fetch(base.appendingPathComponent("conflux/api/hubs"))
                if case let .answered(200, body) = found {
                    hubs = (try? JSON.decoder.decode(FoundHubs.self, from: body))?.hubs ?? []
                } else {
                    hubs = []
                }
            }
        case let .answered(status, body):
            dashboard = nil
            if let node, !node.hubLink.connected {
                phase = .hubUnreachable
            } else {
                let error = (try? JSONSerialization.jsonObject(with: body) as? [String: Any])?["error"] as? String
                phase = .problem(withoutHubAddress(error ?? "conflux answered HTTP \(status)."))
            }
        }
    }

    private func withoutHubAddress(_ text: String) -> String {
        node.map { text.replacingOccurrences(of: " (\($0.hub))", with: "") } ?? text
    }

    private func fetch(_ url: URL) async -> Fetched {
        var request = URLRequest(url: url)
        let known = seen[url]
        if let known { request.setValue(known.tag, forHTTPHeaderField: "If-None-Match") }
        do {
            let (body, response) = try await session.data(for: request)
            let answer = response as? HTTPURLResponse
            let status = answer?.statusCode ?? 0
            if status == 304, let latest = seen[url] ?? known { return .answered(status: 200, body: latest.body) }
            if status == 200 {
                seen[url] = answer?.value(forHTTPHeaderField: "ETag").map { (tag: $0, body: body) }
            }
            return .answered(status: status, body: body)
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

    var pools: [Pool] {
        Dictionary(grouping: accounts, by: \.provider)
            .map { Pool(provider: $0.key, accounts: $0.value) }
            .sorted { $0.provider < $1.provider }
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
            if !enabled.isEmpty && readyCount == 0 { return .systemOrange }
            return guestsWaiting.isEmpty && latestVersion == nil ? nil : .controlAccentColor
        case .waiting:
            if updateFailure != nil { return .systemRed }
            return latestVersion == nil ? nil : .controlAccentColor
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

    var guestsWaiting: [Machine] {
        dashboard?.machines.filter { $0.access == "pending" } ?? []
    }

    var isDown: Bool {
        phase == .notRunning || phase == .hubUnreachable
    }

    private var unsettled: Bool {
        if case .problem = phase { return true }
        return isDown
    }

    var needsAttention: Bool {
        attention != nil
    }

    func isThisMac(_ machine: Machine) -> Bool {
        node.map { machine.hostname == $0.hostname && (machine.guest ?? false) == ($0.guest ?? false) }
            ?? (machine.role == "hub" || machine.role == "waiting")
    }

    var machines: [Machine] {
        dashboard?.machines ?? waiting?.machines ?? []
    }

    var showsRouting: Bool {
        switch phase {
        case .ready, .notApproved, .hubUnreachable, .notRunning, .problem: return true
        default: return false
        }
    }

    var showsUpdateCard: Bool {
        updating || updateFailure != nil || latestVersion != nil
    }

    var latestVersion: String? {
        guard let latest = dashboard?.latestVersion ?? waiting?.latestVersion, !outdatedMachines.isEmpty else { return nil }
        return latest
    }

    var outdatedMachines: [Machine] {
        machines.filter { $0.outdated && $0.online && !$0.isUpdating && canUpdate($0) }
    }

    func canUpdate(_ machine: Machine) -> Bool {
        isThisMac(machine) || (node?.guest != true && machine.guest != true)
    }

    var upToDate: Bool {
        checkedForUpdates && !updating && !machines.contains { $0.outdated && canUpdate($0) }
    }

    var updating: Bool {
        machines.contains { $0.online && $0.isUpdating }
    }

    var inApplications: Bool {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let folder = bundle.deletingLastPathComponent().path
        return ["/Applications", "\(home)/Applications"].contains(folder)
            && FileManager.default.isWritableFile(atPath: folder)
    }

    var updateFailure: String? {
        guard let machine = machines.first(where: { $0.online && $0.update?.state == "failed" }) else {
            return nil
        }
        return "\(machine.hostname): \(machine.update?.error ?? "update failed")"
    }

    var roleLine: String {
        switch phase {
        case .ready:
            let version = latestVersion == nil ? dashboard.map { "v\($0.hub.version)" } : nil
            if node != nil { return version ?? "" }
            return ["This Mac is the hub", version].compactMap { $0 }.joined(separator: " · ")
        case .hubUnreachable, .notApproved:
            return "Linked to a hub"
        case .waiting:
            return "Waiting to be linked"
        case .notRunning:
            switch local.config("role") {
            case "hub": return "This Mac is the hub"
            case "node": return "Linked to a hub"
            case "waiting": return "Waiting to be linked"
            default: return runningVersion.map { "v\($0)" } ?? ""
            }
        case .notSetUp, .outsideApplications:
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
                failure = withoutHubAddress(error.localizedDescription)
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
            guard !self.local.devCopy else {
                throw Command.Failure(message: "This is a dev copy. It has no background service to set up.")
            }
            guard self.inApplications else {
                throw Command.Failure(message: "Move Conflux to the Applications folder in your home folder first.")
            }
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

    private func post(_ path: String, _ body: [String: Any]) async throws -> [String: Any] {
        var request = URLRequest(url: local.base(port: local.port).appendingPathComponent(path))
        request.httpMethod = "POST"
        request.timeoutInterval = 30
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (answer, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        let fields = (try? JSONSerialization.jsonObject(with: answer)) as? [String: Any] ?? [:]
        guard (200..<300).contains(status) else {
            throw Command.Failure(message: fields["error"] as? String ?? "conflux answered HTTP \(status).")
        }
        return fields
    }

    func checkForUpdates() {
        perform("Checking…") {
            self.checkedForUpdates = false
            _ = try await self.post("conflux/api/update/check", [:])
            await self.refresh()
            self.checkedForUpdates = true
        }
    }

    func updateAll() {
        perform("Starting update…") {
            let machines = self.outdatedMachines.map(\.hostname)
            guard !machines.isEmpty else { throw Command.Failure(message: "Nothing to update right now.") }
            let answer = try await self.post("conflux/api/update", ["machines": machines])
            let started = answer["started"] as? [String] ?? []
            let skipped = answer["skipped"] as? [[String: Any]] ?? []
            if started.isEmpty, let first = skipped.first {
                let machine = first["machine"] as? String ?? "A machine"
                throw Command.Failure(message: "\(machine) was skipped: \(first["reason"] as? String ?? "no reason given")")
            }
        }
    }

    func link(to hub: FoundHub) {
        perform("Linking…") {
            _ = try await self.post("conflux/api/join", ["address": hub.address])
            let node = self.local.base(port: self.local.port).appendingPathComponent("conflux/node")
            for _ in 0..<20 {
                if case let .answered(200, body) = await self.fetch(node),
                   hub.guest != true || (try? JSON.decoder.decode(NodeInfo.self, from: body))?.access != nil {
                    break
                }
                try? await Task.sleep(nanoseconds: 250_000_000)
            }
            await self.refresh()
        }
    }

    func decide(_ guest: Machine, approve: Bool) {
        guard let id = guest.id else { return }
        perform(approve ? "Approving…" : "Turning down…") {
            _ = try await self.post("conflux/api/guests/\(id)", ["approve": approve])
        }
    }

    func askAgain() {
        perform("Asking…") { _ = try await self.post("conflux/node/ask", [:]) }
    }

    func openDashboard() {
        NSWorkspace.shared.open(dashboardURL)
    }

    func revealInFinder() {
        NSWorkspace.shared.activateFileViewerSelecting([bundle])
    }
}
