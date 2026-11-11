import Foundation

struct Dashboard: Decodable {
    struct HubInfo: Decodable {
        let name: String
        let version: String
    }

    let hub: HubInfo
    let latestVersion: String?
    let accounts: [Account]
    let machines: [Machine]
}

struct Account: Decodable, Identifiable {
    let id: String
    let provider: String
    let email: String?
    let plan: String?
    let enabled: Bool
    let allowOverage: Bool
    let needsLogin: String?
    let billingBlocked: String?
    let cooldowns: [Cooldown]
    let usage: Usage
}

struct Cooldown: Decodable {
    let scope: String
    let until: Date
    let reason: String
}

struct Usage: Decodable {
    let windows: [UsageWindow]
    let overageInUse: Bool
    let updatedAt: Date?
}

struct UsageWindow: Decodable, Identifiable {
    let key: String
    let usedPct: Double
    let resetsAt: Date?
    let durationSecs: Int?
    let modelScope: String?

    var id: String { key }
}

struct Machine: Decodable {
    struct Update: Decodable {
        let state: String
        let version: String
        let error: String?
    }

    let hostname: String
    let version: String
    let role: String
    let online: Bool
    let outdated: Bool
    let update: Update?
}

struct NodeInfo: Decodable {
    struct Link: Decodable {
        let connected: Bool
        let lastError: String?
    }

    let hub: String
    let hostname: String
    let hubLink: Link
}

enum Fetched {
    case answered(status: Int, body: Data)
    case unreachable
}

struct Local {
    let configDir: URL
    let launchAgents: URL
    let cli: URL

    static func fromEnvironment(bundle: URL) -> Local {
        let env = ProcessInfo.processInfo.environment
        let home = FileManager.default.homeDirectoryForCurrentUser
        let configDir: URL
        if let base = env["CONFLUX_HOME"] {
            configDir = URL(fileURLWithPath: base).appendingPathComponent("config")
        } else {
            let xdg = env["XDG_CONFIG_HOME"].map { URL(fileURLWithPath: $0) } ?? home.appendingPathComponent(".config")
            configDir = xdg.appendingPathComponent("conflux")
        }
        return Local(
            configDir: configDir,
            launchAgents: home.appendingPathComponent("Library/LaunchAgents"),
            cli: bundle.appendingPathComponent("Contents/Helpers/conflux")
        )
    }

    func config(_ key: String) -> String? {
        let text = (try? String(contentsOf: configDir.appendingPathComponent("config.toml"), encoding: .utf8)) ?? ""
        for line in text.split(separator: "\n") {
            let parts = line.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            if parts.count == 2, parts[0] == key { return parts[1].trimmingCharacters(in: CharacterSet(charactersIn: "\"")) }
        }
        return nil
    }

    var port: Int {
        config("port").flatMap(Int.init) ?? 8317
    }

    var routing: Bool? {
        guard let data = try? Data(contentsOf: configDir.appendingPathComponent("joined.json")),
              let joined = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }
        return joined["active"] as? Bool
    }

    var serviceInstalled: Bool {
        FileManager.default.fileExists(atPath: servicePlist.path)
    }

    var servicePlist: URL {
        launchAgents.appendingPathComponent("dev.conflux.plist")
    }

    var serviceTarget: String {
        "gui/\(getuid())/dev.conflux"
    }

    func base(port: Int) -> URL {
        URL(string: "http://127.0.0.1:\(port)")!
    }
}

enum Command {
    struct Failure: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    static func run(_ executable: URL, _ arguments: [String]) async throws -> String {
        try await withCheckedThrowingContinuation { continuation in
            let process = Process()
            let output = Pipe()
            process.executableURL = executable
            process.arguments = arguments
            process.standardInput = FileHandle.nullDevice
            process.standardOutput = output
            process.standardError = output
            process.terminationHandler = { finished in
                let text = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                if finished.terminationStatus == 0 {
                    continuation.resume(returning: text)
                } else {
                    let last = text.split(separator: "\n").last.map(String.init)
                    continuation.resume(throwing: Failure(message: last ?? "\(executable.lastPathComponent) failed"))
                }
            }
            do {
                try process.run()
            } catch {
                continuation.resume(throwing: error)
            }
        }
    }
}

enum JSON {
    static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        decoder.dateDecodingStrategy = .custom { decoder in
            let text = try decoder.singleValueContainer().decode(String.self)
            guard let date = parseDate(text) else {
                throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "bad date \(text)"))
            }
            return date
        }
        return decoder
    }()

    static func parseDate(_ text: String) -> Date? {
        var trimmed = text
        if let dot = text.firstIndex(of: "."),
           let zone = text[dot...].firstIndex(where: { $0 == "Z" || $0 == "+" || $0 == "-" })
        {
            trimmed = String(text[..<dot]) + String(text[zone...])
        }
        return ISO8601DateFormatter().date(from: trimmed)
    }
}
