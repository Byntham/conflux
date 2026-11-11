// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Conflux",
    platforms: [.macOS(.v13)],
    targets: [.executableTarget(name: "Conflux", path: "Sources/Conflux")]
)
