import XCTest
#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif

final class ControlPlaneKitTests: XCTestCase {
    private var fixtureBundle: Bundle {
        #if CONTROL_PLANE_XCODE_TESTS
        return Bundle(for: Self.self)
        #else
        return .module
        #endif
    }
    func testBrokerAccountBasePathIsPreservedForAllWebSockets() throws {
        let client = try APIClient(address: "https://host.example:9847/accounts/alice", token: "device-secret")
        for path in ["v1/events", "v1/terminal", "v1/desktop/video"] {
            let socket = client.socket(path)
            XCTAssertEqual(socket.originalRequest?.url?.scheme, "wss")
            XCTAssertEqual(socket.originalRequest?.url?.path, "/accounts/alice/" + path)
            XCTAssertEqual(socket.originalRequest?.value(forHTTPHeaderField: "X-Device-Token"), "device-secret")
            XCTAssertFalse(socket.originalRequest?.url?.absoluteString.contains("device-secret") ?? true)
            socket.cancel(with: .goingAway, reason: nil)
        }
    }
    func testExecutionDestinationsKeepLinuxPathsAndDistroIdentity() throws {
        let data = Data(#"[{"execution_target":{"kind":"native"},"name":"Host OS"},{"execution_target":{"kind":"wsl","distribution":"Ubuntu-24.04"},"user":"alice","roots":["/home/alice/projects"]}]"#.utf8)
        let targets = try JSONDecoder().decode([ExecutionDestination].self, from: data)
        XCTAssertEqual(targets.map(\.id), ["native", "wsl:Ubuntu-24.04"])
        XCTAssertEqual(targets[1].roots, ["/home/alice/projects"])
        XCTAssertEqual(targets[1].execution_target["kind"], .string("wsl"))
    }
    func testVideoToolboxDecodesSyntheticEncoderFixture() async throws {
        let url=try XCTUnwrap(fixtureBundle.url(forResource:"synthetic-h264",withExtension:"cpv"))
        let packet=try H264Packet(data:Data(contentsOf:url))
        let frame=try await H264VideoDecoder().decode(packet)
        XCTAssertEqual(frame.image.width,64);XCTAssertEqual(frame.image.height,64)
        XCTAssertEqual(frame.header.geometry_version,1)
    }
    func testVideoRejectsOversizedAndTruncatedPackets() throws {
        let url=try XCTUnwrap(fixtureBundle.url(forResource:"synthetic-h264",withExtension:"cpv"))
        var bytes=try Data(contentsOf:url);bytes.removeLast()
        XCTAssertThrowsError(try H264Packet(data:bytes))
        XCTAssertThrowsError(try H264Packet(data:Data("CPV1".utf8)+Data([0,0,0,1,255,255,255,255])))
    }
    func testRemoteTransportRequiresTLS() throws {
        XCTAssertThrowsError(try APIClient(address: "http://10.0.0.2:7443", token: "secret"))
        XCTAssertNoThrow(try APIClient(address: "https://machine.example:7443", token: "secret"))
        XCTAssertNoThrow(try APIClient(address: "http://127.0.0.1:7443", token: "secret"))
        XCTAssertThrowsError(try APIClient(address: "https://user:secret@example.com", token: "secret"))
    }
    func testDecodesProviderPayloadWithoutLosingFields() throws {
        let data = Data(#"{"sequence":42,"session_id":"s","kind":"assistant","payload":{"text":"hello","tokens":3},"created_at":123}"#.utf8)
        let event = try JSONDecoder().decode(SessionEvent.self, from: data)
        XCTAssertEqual(event.sequence, 42)
        XCTAssertEqual(event.payload.text, "hello")
        XCTAssertEqual(event.payload["tokens"], .number(3))
        XCTAssertEqual(try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(event.payload)), event.payload)
    }
    func testHostContractUsesNumericProtocolVersion() throws {
        let data = Data(#"{"id":"h","name":"Host","os":"macos","protocol_version":1,"providers":[{"name":"codex","available":true,"version":"1.0","detail":null}],"desktop":{"available":false}}"#.utf8)
        let host = try JSONDecoder().decode(HostInfo.self, from: data)
        XCTAssertEqual(host.protocol_version, 1)
        XCTAssertEqual(host.providers.first?.name, "codex")
    }
    func testRequestAuthenticationQueryAndConflict() async throws {
        let config = URLSessionConfiguration.ephemeral; config.protocolClasses = [StubProtocol.self]
        let client = try APIClient(address: "https://host.example:7443", token: "device-token", session: URLSession(configuration: config))
        StubProtocol.handler = { request in
            XCTAssertEqual(request.value(forHTTPHeaderField: "X-Device-Token"), "device-token")
            XCTAssertEqual(URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems?.first?.value, "a b/λ")
            return (409, Data(#"{"error":"File changed since read"}"#.utf8))
        }
        do { let _: JSONValue = try await client.request("v1/workspace/read", query: ["path": "a b/λ"]); XCTFail("Expected conflict") }
        catch { XCTAssertTrue(error.localizedDescription.contains("File changed since read")) }
    }
}

private final class StubProtocol: URLProtocol {
    static var handler: ((URLRequest) -> (Int, Data))?
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        guard let (code, data) = Self.handler?(request), let url = request.url else { return }
        client?.urlProtocol(self, didReceive: HTTPURLResponse(url: url, statusCode: code, httpVersion: nil, headerFields: nil)!, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data); client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}
