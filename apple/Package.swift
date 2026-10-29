// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "ControlPlane",
    platforms: [.macOS("15.0"), .iOS("18.0")],
    products: [.library(name: "ControlPlaneKit", targets: ["ControlPlaneKit"]), .executable(name: "ControlPlaneApp", targets: ["ControlPlaneApp"])],
    dependencies: [
        .package(url: "https://github.com/grpc/grpc-swift-2.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-nio-transport.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-protobuf.git", from: "2.0.0"),
        .package(url: "https://github.com/migueldeicaza/SwiftTerm.git", exact: "1.19.0"),
    ],
    targets: [
        .target(name: "ControlPlaneKit", dependencies: [
            .product(name: "GRPCCore", package: "grpc-swift-2"),
            .product(name: "GRPCNIOTransportHTTP2", package: "grpc-swift-nio-transport"),
            .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
        ]),
        .executableTarget(name: "ControlPlaneApp", dependencies: ["ControlPlaneKit", .product(name:"SwiftTerm",package:"SwiftTerm")],resources:[.process("PrivacyInfo.xcprivacy")]),
        .testTarget(name: "ControlPlaneKitTests", dependencies: ["ControlPlaneKit", .product(name:"SwiftTerm",package:"SwiftTerm")], resources:[.process("Fixtures")])
    ]
)
