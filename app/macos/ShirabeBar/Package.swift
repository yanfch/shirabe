// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "ShirabeBar",
    platforms: [
        .macOS(.v13)
    ],
    products: [
        .executable(name: "ShirabeBar", targets: ["ShirabeBar"])
    ],
    targets: [
        .executableTarget(
            name: "ShirabeBar",
            resources: [
                .process("Resources")
            ]
        )
    ]
)
