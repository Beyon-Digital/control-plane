import XCTest

#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif

final class OrchestratorTests: XCTestCase {
  private let id = "7E65C303-4F21-4D0B-B92F-96D5D9F9237F"

  func testReadOnlyCatalogAndDispatcherBothDenyMutations() async throws {
    let tools = OrchestratorTools(allowManagement: false) { _, _, _, _ in
      XCTFail("Denied call reached host")
      return .null
    }
    XCTAssertFalse(tools.definitions.contains { $0.name == "prompt_session" })
    do {
      _ = try await tools.execute(
        InferenceToolCall(id: "c", name: "prompt_session", arguments: "{}"))
      XCTFail("Mutation should be denied")
    } catch { XCTAssertTrue(error.localizedDescription.contains("permissions")) }
  }

  func testCreationKeepsApprovalPolicyAndWSLTarget() async throws {
    let capture = ToolCapture()
    let tools = OrchestratorTools(allowManagement: true) { path, method, query, body in
      await capture.record(path: path, method: method, query: query, body: body)
      return .object(["id": .string("new")])
    }
    let args =
      #"{"provider":"codex","workspace":"/home/alice/project","title":"Implement","execution_target":{"kind":"wsl","distribution":"Ubuntu"}}"#
    _ = try await tools.execute(InferenceToolCall(id: "c", name: "create_session", arguments: args))
    let recorded = await capture.value
    XCTAssertEqual(recorded?.path, "v1/sessions")
    XCTAssertEqual(recorded?.method, "POST")
    XCTAssertEqual(recorded?.body?["trusted"], .bool(false))
    XCTAssertEqual(recorded?.body?["execution_target"]?["distribution"], .string("Ubuntu"))
    XCTAssertNotNil(recorded?.body?["request_id"])
  }

  func testPathInjectionAndExtraPrivilegeArgumentsNeverReachHost() async throws {
    let tools = OrchestratorTools(allowManagement: true) { _, _, _, _ in
      XCTFail("Invalid arguments reached host")
      return .null
    }
    for args in [
      #"{"session_id":"../devices/x/revoke","text":"task"}"#,
      "{\"session_id\":\"\(id)\",\"text\":\"task\",\"trusted\":true}",
    ] {
      do {
        _ = try await tools.execute(
          InferenceToolCall(id: "c", name: "prompt_session", arguments: args))
        XCTFail("Invalid arguments accepted")
      } catch {}
    }
  }

  func testEventSummaryPreservesLatestCursor() async throws {
    let tools = OrchestratorTools(allowManagement: false) { _, _, _, _ in
      .array((1...50).map { .object(["sequence": .number(Double($0))]) })
    }
    let result = try await tools.execute(
      InferenceToolCall(
        id: "c", name: "session_events", arguments: "{\"session_id\":\"\(id)\",\"after\":0}"))
    XCTAssertEqual(result["last_sequence"], .number(50))
    XCTAssertEqual(result["omitted_earlier"], .bool(true))
    guard case .array(let events) = result["events"] else { return XCTFail("Missing events") }
    XCTAssertEqual(events.count, 40)
    XCTAssertEqual(events.first?["sequence"], .number(11))
  }

  func testPlainChatNeedsNoHostAndReplacesStaleSystemScope() async throws {
    let engine = OrchestratorEngine(client: nil, allowManagement: false) {
      _, messages, tools, delta in
      XCTAssertTrue(tools.isEmpty)
      XCTAssertEqual(messages.filter { $0.role == "system" }.count, 1)
      XCTAssertFalse(messages[0].content.contains("stale scope"))
      XCTAssertTrue(messages[0].content.contains("No host is connected"))
      delta("Hello")
      return InferenceReply(text: "Hello", toolCalls: [])
    }
    let result = try await engine.run(
      profile: ModelProfile(model: "fixture"),
      messages: [
        InferenceMessage(role: "system", content: "stale scope"),
        InferenceMessage(role: "user", content: "Hello"),
      ], onDelta: { _ in }, onTool: { _ in })
    XCTAssertEqual(result.last?.content, "Hello")
  }

  func testToolLoopReturnsResultsAndThenFinalAnswer() async throws {
    let engine = OrchestratorEngine(client: nil, allowManagement: false) { _, messages, _, _ in
      if messages.last?.role == "tool" {
        XCTAssertTrue(messages.last?.content.contains("unavailable") ?? false)
        return InferenceReply(text: "Connect a host first.", toolCalls: [])
      }
      return InferenceReply(
        text: "", toolCalls: [InferenceToolCall(id: "c", name: "list_sessions", arguments: "{}")])
    }
    let result = try await engine.run(
      profile: ModelProfile(model: "fixture"),
      messages: [InferenceMessage(role: "user", content: "List agents")], onDelta: { _ in },
      onTool: { _ in })
    XCTAssertEqual(result.map(\.role), ["system", "user", "assistant", "tool", "assistant"])
    XCTAssertEqual(result[3].toolCallID, "c")
  }

  func testRepeatedToolCallIDStopsBeforeRepeatingAction() async throws {
    let engine = OrchestratorEngine(client: nil, allowManagement: false) { _, _, _, _ in
      InferenceReply(
        text: "",
        toolCalls: [InferenceToolCall(id: "same-id", name: "list_sessions", arguments: "{}")])
    }
    do {
      _ = try await engine.run(
        profile: ModelProfile(model: "fixture"),
        messages: [InferenceMessage(role: "user", content: "List agents")], onDelta: { _ in },
        onTool: { _ in })
      XCTFail("Repeated call ID accepted")
    } catch OrchestratorError.limit {}
  }

  func testOversizedConversationFailsBeforeInference() async throws {
    let engine = OrchestratorEngine(client: nil, allowManagement: false) { _, _, _, _ in
      XCTFail("Oversized context reached inference")
      return InferenceReply(text: "", toolCalls: [])
    }
    do {
      _ = try await engine.run(
        profile: ModelProfile(model: "fixture"),
        messages: [
          InferenceMessage(role: "user", content: String(repeating: "x", count: 256 * 1024))
        ], onDelta: { _ in }, onTool: { _ in })
      XCTFail("Oversized context accepted")
    } catch { XCTAssertTrue(error.localizedDescription.contains("conversation is full")) }
  }

  func testCancellationAfterInferencePreventsToolDispatch() async throws {
    let engine = OrchestratorEngine(client: nil, allowManagement: true) { _, _, _, _ in
      withUnsafeCurrentTask { $0?.cancel() }
      return InferenceReply(
        text: "", toolCalls: [InferenceToolCall(id: "c", name: "create_session", arguments: "{}")])
    }
    let turn = Task {
      try await engine.run(
        profile: ModelProfile(model: "fixture"),
        messages: [InferenceMessage(role: "user", content: "Start agent")], onDelta: { _ in },
        onTool: { _ in XCTFail("Canceled turn dispatched a tool") })
    }
    do {
      _ = try await turn.value
      XCTFail("Canceled turn succeeded")
    } catch is CancellationError {}
  }

  func testOpaqueResponsesItemsSurviveToolRoundTrip() async throws {
    let opaque: [JSONValue] = [
      .object([
        "type": .string("reasoning"), "encrypted_content": .string("synthetic-opaque-state"),
      ])
    ]
    let engine = OrchestratorEngine(client: nil, allowManagement: false) { _, messages, _, _ in
      if messages.last?.role == "tool" {
        XCTAssertEqual(messages.first(where: { $0.role == "assistant" })?.responsesOutput, opaque)
        return InferenceReply(text: "Done", toolCalls: [])
      }
      return InferenceReply(
        text: "", toolCalls: [InferenceToolCall(id: "c", name: "list_sessions", arguments: "{}")],
        responsesOutput: opaque)
    }
    let result = try await engine.run(
      profile: ModelProfile(model: "fixture"),
      messages: [InferenceMessage(role: "user", content: "Inspect")], onDelta: { _ in },
      onTool: { _ in })
    XCTAssertEqual(result.first(where: { $0.role == "assistant" })?.responsesOutput, opaque)
  }
}

private actor ToolCapture {
  struct Value: Sendable {
    let path: String
    let method: String
    let query: [String: String]
    let body: [String: JSONValue]?
  }
  var value: Value?
  func record(path: String, method: String, query: [String: String], body: [String: JSONValue]?) {
    value = Value(path: path, method: method, query: query, body: body)
  }
}
