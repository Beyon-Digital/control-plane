import XCTest

#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif

final class ModelInferenceTests: XCTestCase {
  private func profile(_ api: ModelAPI = .responses) -> ModelProfile {
    ModelProfile(name: "Fixture", baseURL: "https://model.invalid/v1", model: "fixture", api: api)
  }
  private func client() -> ModelInference {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [ModelFixtureProtocol.self]
    return ModelInference(configuration: config, tokenProvider: { _ in "fixture-secret" })
  }
  private func stream(_ events: [[String: Any]]) throws -> Data {
    Data(
      try events.map {
        "data: " + String(decoding: try JSONSerialization.data(withJSONObject: $0), as: UTF8.self)
          + "\n\n"
      }.joined().utf8)
  }
  func testEndpointValidationRejectsCredentialAndRemoteHTTPAddresses() throws {
    for address in [
      "http://10.0.0.1/v1", "http://localhost.evil/v1", "https://user:secret@host.invalid/v1",
      "https://host.invalid/v1?key=secret", "https://host.invalid/v1#fragment",
    ] {
      var p = profile()
      p.baseURL = address
      XCTAssertThrowsError(try p.validatedURL())
    }
    for address in [
      "http://localhost:8080/v1", "http://127.0.0.1:8080/v1", "http://[::1]:8080/v1",
      "https://model.invalid/v1",
    ] {
      var p = profile()
      p.baseURL = address
      XCTAssertNoThrow(try p.validatedURL())
    }
    var p = profile()
    p.auth = .chatGPT
    p.chatGPTAccountID = "account"
    XCTAssertThrowsError(try p.validatedURL())
    p.baseURL = "https://api.openai.com/v1"
    XCTAssertNoThrow(try p.validatedURL())
    p.api = .chatCompletions
    XCTAssertThrowsError(try p.validatedURL())
  }
  func testResponsesUsesCallIDAndPreservesOpaqueReasoningAndPhase() async throws {
    let output: [[String: Any]] = [
      [
        "type": "reasoning", "id": "reasoning-1", "summary": [],
        "encrypted_content": "opaque-fixture",
      ],
      [
        "type": "message", "id": "message-1", "role": "assistant", "phase": "commentary",
        "content": [["type": "output_text", "text": "Checking."]],
      ],
      [
        "type": "function_call", "id": "item-1", "call_id": "call-1", "name": "list_sessions",
        "arguments": "{}",
      ],
    ]
    let data = try stream([
      ["type": "response.output_text.delta", "delta": "Checking."],
      [
        "type": "response.output_item.added", "output_index": 2,
        "item": [
          "type": "function_call", "id": "item-1", "call_id": "call-1", "name": "list_sessions",
          "arguments": "",
        ],
      ],
      [
        "type": "response.function_call_arguments.delta", "output_index": 2, "item_id": "item-1",
        "delta": "{}",
      ],
      ["type": "response.output_item.done", "output_index": 2, "item": output[2]],
      ["type": "response.completed", "response": ["status": "completed", "output": output]],
    ])
    ModelFixtureProtocol.handler = { request in
      XCTAssertEqual(request.url?.path, "/v1/responses")
      XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer fixture-secret")
      let body = try Self.body(request)
      XCTAssertEqual(body["store"] as? Bool, false)
      XCTAssertEqual(body["stream"] as? Bool, true)
      XCTAssertEqual(body["max_output_tokens"] as? Int, 8192)
      return (200, "text/event-stream", data)
    }
    let reply = try await client().complete(
      profile: profile(), messages: [.init(role: "user", content: "Inspect sessions")])
    XCTAssertEqual(reply.text, "Checking.")
    XCTAssertEqual(reply.toolCalls.first?.id, "call-1")
    XCTAssertEqual(reply.responsesOutput.count, 3)
    XCTAssertEqual(reply.responsesOutput[0]["encrypted_content"], .string("opaque-fixture"))
    ModelFixtureProtocol.handler = { request in
      let body = try Self.body(request)
      let input = try XCTUnwrap(body["input"] as? [[String: Any]])
      XCTAssertEqual(input[0]["encrypted_content"] as? String, "opaque-fixture")
      XCTAssertEqual(input[1]["phase"] as? String, "commentary")
      XCTAssertEqual(input[2]["call_id"] as? String, "call-1")
      XCTAssertEqual(input[3]["call_id"] as? String, "call-1")
      XCTAssertEqual(input.count, 4)
      return (
        200, "text/event-stream",
        try self.stream([
          ["type": "response.completed", "response": ["status": "completed", "output": []]]
        ])
      )
    }
    _ = try await client().complete(
      profile: profile(),
      messages: [
        .init(
          role: "assistant", content: reply.text, toolCalls: reply.toolCalls,
          responsesOutput: reply.responsesOutput),
        .init(role: "tool", content: "[]", toolCallID: "call-1"),
      ])
  }
  func testResponsesFailureAfterDeltaNeverReturnsToolCalls() async throws {
    let data = try stream([
      ["type": "response.output_text.delta", "delta": "partial"],
      [
        "type": "response.failed",
        "response": [
          "error": [
            "code": "subscription_sharing_usage_limit_exceeded", "message": "fixture-secret",
          ]
        ],
      ],
    ])
    ModelFixtureProtocol.handler = { _ in (200, "text/event-stream", data) }
    do {
      _ = try await client().complete(
        profile: profile(), messages: [.init(role: "user", content: "x")])
      XCTFail("Expected terminal failure")
    } catch {
      XCTAssertEqual(
        error as? ModelInferenceError, .provider("subscription_sharing_usage_limit_exceeded"))
      XCTAssertFalse(error.localizedDescription.contains("fixture-secret"))
    }
  }
  func testResponsesRequiresCompletedRatherThanSocketEOF() async throws {
    let data = try stream([["type": "response.output_text.delta", "delta": "partial"]])
    ModelFixtureProtocol.handler = { _ in (200, "text/event-stream", data) }
    do {
      _ = try await client().complete(
        profile: profile(), messages: [.init(role: "user", content: "x")])
      XCTFail("Expected incomplete stream")
    } catch { XCTAssertEqual(error as? ModelInferenceError, .incomplete) }
  }
  func testChatCompletionsAccumulatesToolArgumentsAndRequiresDone() async throws {
    let first = Data(
      #"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"chat-call","type":"function","function":{"name":"inspect","arguments":"{\"path\":"}}]}}]}"#
        .utf8)
    let second = Data(
      #"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#
        .utf8)
    var data = first
    data.append(Data("\n\n".utf8))
    data.append(second)
    data.append(Data("\n\ndata: [DONE]\n\n".utf8))
    ModelFixtureProtocol.handler = { request in
      XCTAssertEqual(request.url?.path, "/v1/chat/completions")
      return (200, "text/event-stream", data)
    }
    let reply = try await client().complete(
      profile: profile(.chatCompletions), messages: [.init(role: "user", content: "x")])
    XCTAssertEqual(
      reply.toolCalls,
      [InferenceToolCall(id: "chat-call", name: "inspect", arguments: "{\"path\":\"a\"}")])
  }
  func testModelDiscoveryUsesDraftCredentialWithoutSavingAndNeverRetries() async throws {
    var count = 0
    ModelFixtureProtocol.handler = { request in
      count += 1
      XCTAssertEqual(request.url?.path, "/v1/models")
      XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer draft-key")
      return (200, "application/json", Data(#"{"data":[{"id":"one"},{"id":"two"}]}"#.utf8))
    }
    let models = try await client().models(profile: profile(), apiKeyOverride: "draft-key")
    XCTAssertEqual(models, ["one", "two"])
    XCTAssertEqual(count, 1)
    ModelFixtureProtocol.handler = { _ in
      count += 1
      return (
        401, "application/json",
        Data(#"{"error":{"code":"fixture-secret","message":"fixture-secret"}}"#.utf8)
      )
    }
    do {
      _ = try await client().models(profile: profile())
      XCTFail("Expected HTTP failure")
    } catch { XCTAssertFalse(error.localizedDescription.contains("fixture-secret")) }
    XCTAssertEqual(count, 2)
  }
  func testDraftEndpointCannotReuseStoredCredential() throws {
    var p = profile()
    let record = String(
      decoding: try JSONSerialization.data(withJSONObject: [
        "baseURL": "https://model.invalid/v1", "token": "bound-secret",
      ]), as: UTF8.self)
    XCTAssertEqual(ModelProfile.storedAPIKey(for: p, load: { _ in record }), "bound-secret")
    p.baseURL = "https://other.invalid/v1"
    XCTAssertNil(ModelProfile.storedAPIKey(for: p, load: { _ in record }))
    p.baseURL = "https://model.invalid/other"
    XCTAssertNil(ModelProfile.storedAPIKey(for: p, load: { _ in record }))
  }
  func testLegacyChatServerUsesExplicitTokenLimitFieldWithoutRetry() async throws {
    var p = profile(.chatCompletions)
    p.chatTokenParameter = .maxTokens
    ModelFixtureProtocol.handler = { request in
      let body = try Self.body(request)
      XCTAssertEqual(body["max_tokens"] as? Int, 8192)
      XCTAssertNil(body["max_completion_tokens"])
      return (
        200, "text/event-stream",
        Data(
          "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
            .utf8)
      )
    }
    let reply = try await client().complete(
      profile: p, messages: [.init(role: "user", content: "Hi")])
    XCTAssertEqual(reply.text, "Hi")
  }
  func testSSEBoundsLinesAndIgnoresComments() throws {
    var parser = InferenceSSEParser()
    var result: String?
    for byte in Data(": keepalive\r\ndata: {\"x\":1}\r\n\r\n".utf8) {
      if let event = try parser.feed(byte) { result = event }
    }
    XCTAssertEqual(result, "{\"x\":1}")
    for _ in 0..<1024 * 1024 { _ = try parser.feed(65) }
    XCTAssertThrowsError(try parser.feed(65))
  }
  @MainActor func testProfileMetadataContainsNoAPIKeyAndRevisionChanges() throws {
    let name = "model-fixture-" + UUID().uuidString
    let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
    defer { defaults.removePersistentDomain(forName: name) }
    let store = ModelProfileStore(defaults: defaults)
    let p = ModelProfile(name: "Local", baseURL: "http://127.0.0.1:8000/v1")
    try store.save(p)
    let revision = store.revision
    try store.save(p)
    XCTAssertGreaterThan(store.revision, revision)
    XCTAssertEqual(store.selectedID, p.id)
    let metadata = try XCTUnwrap(defaults.data(forKey: "controlplane.orchestrator.models.v1"))
    XCTAssertFalse(String(decoding: metadata, as: UTF8.self).contains("apiKey\":"))
    store.delete(id: p.id)
    XCTAssertTrue(store.profiles.isEmpty)
  }
  private static func body(_ request: URLRequest) throws -> [String: Any] {
    let data: Data
    if let body = request.httpBody {
      data = body
    } else if let stream = request.httpBodyStream {
      stream.open()
      defer { stream.close() }
      var result = Data()
      var bytes = [UInt8](repeating: 0, count: 4096)
      while stream.hasBytesAvailable {
        let n = stream.read(&bytes, maxLength: bytes.count)
        if n <= 0 { break }
        result.append(contentsOf: bytes.prefix(n))
      }
      data = result
    } else {
      throw ModelInferenceError.invalidResponse
    }
    return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
  }
}
private final class ModelFixtureProtocol: URLProtocol {
  static var handler: ((URLRequest) throws -> (Int, String, Data))?
  override class func canInit(with request: URLRequest) -> Bool { true }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    do {
      guard let handler = Self.handler, let url = request.url else {
        throw ModelInferenceError.invalidResponse
      }
      let (code, type, data) = try handler(request)
      client?.urlProtocol(
        self,
        didReceive: HTTPURLResponse(
          url: url, statusCode: code, httpVersion: nil, headerFields: ["Content-Type": type])!,
        cacheStoragePolicy: .notAllowed)
      client?.urlProtocol(self, didLoad: data)
      client?.urlProtocolDidFinishLoading(self)
    } catch { client?.urlProtocol(self, didFailWithError: error) }
  }
  override func stopLoading() {}
}
