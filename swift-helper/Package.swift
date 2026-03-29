// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "IndexHelper",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "index-helper", targets: ["IndexHelper"]),
    ],
    dependencies: [
        .package(url: "https://github.com/swiftlang/indexstore-db", branch: "swift-6.3-RELEASE"),
    ],
    targets: [
        .executableTarget(
            name: "IndexHelper",
            dependencies: [
                .product(name: "IndexStoreDB", package: "indexstore-db"),
            ]
        ),
    ]
)
