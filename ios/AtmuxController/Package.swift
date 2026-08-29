// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "AtmuxControllerCore",
    platforms: [.macOS(.v12), .iOS(.v17)],
    products: [.library(name: "AtmuxCore", targets: ["AtmuxCore"])],
    targets: [
        .target(
            name: "AtmuxCore",
            path: "AtmuxController",
            exclude: [
                "AtmuxControllerApp.swift", "AppModel.swift", "ContentView.swift",
                "ConnectionView.swift", "SessionsView.swift", "SessionDetailView.swift",
                "PulseView.swift",
            ],
            sources: [
                "Models.swift", "ConnectionProfile.swift", "KeychainStore.swift",
                "ClientIdentity.swift", "APIEndpoint.swift", "APIClient.swift",
            ]
        ),
        .testTarget(
            name: "AtmuxCoreTests",
            dependencies: ["AtmuxCore"],
            path: "AtmuxControllerTests"
        ),
    ]
)
