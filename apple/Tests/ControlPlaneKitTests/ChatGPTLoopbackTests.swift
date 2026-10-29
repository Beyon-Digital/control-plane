import XCTest

#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif

final class ChatGPTLoopbackTests: XCTestCase {
  func testLoopbackIgnoresWrongStateAndMethodBeforeAcceptingCallback() async throws {
    let server = ChatGPTLoopback()
    defer { server.stop() }
    let redirect = try await server.start()
    XCTAssertEqual(redirect.host, "127.0.0.1")
    XCTAssertEqual(redirect.path, "/auth/callback")
    XCTAssertEqual(redirect.scheme, "http")
    XCTAssertNotNil(redirect.port)
    await server.setExpectedState("expected-state")
    let waiting = Task { try await server.waitForCallback() }
    let session = URLSession(configuration: .ephemeral)
    defer { session.invalidateAndCancel() }
    func callback(_ state: String) -> URL {
      var parts = URLComponents(url: redirect, resolvingAgainstBaseURL: false)!
      parts.queryItems = [
        URLQueryItem(name: "state", value: state),
        URLQueryItem(name: "code", value: "synthetic-code"),
        URLQueryItem(name: "client_id", value: "synthetic-client"),
      ]
      return parts.url!
    }
    let (_, wrongState) = try await session.data(from: callback("wrong-state"))
    XCTAssertEqual((wrongState as? HTTPURLResponse)?.statusCode, 400)
    var post = URLRequest(url: callback("expected-state"))
    post.httpMethod = "POST"
    let (_, wrongMethod) = try await session.data(for: post)
    XCTAssertEqual((wrongMethod as? HTTPURLResponse)?.statusCode, 400)
    let (body, success) = try await session.data(from: callback("expected-state"))
    XCTAssertEqual((success as? HTTPURLResponse)?.statusCode, 200)
    XCTAssertFalse(String(decoding: body, as: UTF8.self).contains("synthetic-code"))
    let received = try await waiting.value
    XCTAssertEqual(received, callback("expected-state"))
  }

  func testCancellationReleasesPendingCallback() async throws {
    let server = ChatGPTLoopback()
    _ = try await server.start()
    await server.setExpectedState("expected-state")
    let waiting = Task { try await server.waitForCallback() }
    server.stop()
    do {
      _ = try await waiting.value
      XCTFail("Canceled callback succeeded")
    } catch is CancellationError {}
  }
}
