// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "ember-vz",
    platforms: [
        .macOS(.v13)  // Virtualization.framework Linux boot requires macOS 13+
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser.git", from: "1.3.0"),
    ],
    targets: [
        .executableTarget(
            name: "ember-vz",
            dependencies: [
                .product(name: "ArgumentParser", package: "swift-argument-parser"),
            ],
            path: "Sources/EmberVZ",
            linkerSettings: [
                .linkedFramework("Virtualization"),
            ]
        ),
    ]
)
