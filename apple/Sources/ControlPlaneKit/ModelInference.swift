import Foundation

public struct InferenceToolCall: Codable, Equatable, Sendable {
  public var id: String
  public var name: String
  public var arguments: String
  public init(id: String, name: String, arguments: String) {
    self.id = id
    self.name = name
    self.arguments = arguments
  }
}
public struct InferenceMessage: Codable, Equatable, Sendable {
  public var role: String
  public var content: String
  public var toolCalls: [InferenceToolCall]
  public var toolCallID: String?
  public var responsesOutput: [JSONValue]
  public init(
    role: String, content: String, toolCalls: [InferenceToolCall] = [], toolCallID: String? = nil,
    responsesOutput: [JSONValue] = []
  ) {
    self.role = role
    self.content = content
    self.toolCalls = toolCalls
    self.toolCallID = toolCallID
    self.responsesOutput = responsesOutput
  }
}
public struct InferenceTool: Codable, Equatable, Sendable {
  public var name: String
  public var description: String
  public var parameters: JSONValue
  public init(name: String, description: String, parameters: JSONValue) {
    self.name = name
    self.description = description
    self.parameters = parameters
  }
}
public struct InferenceReply: Codable, Equatable, Sendable {
  public var text: String
  public var toolCalls: [InferenceToolCall]
  public var responsesOutput: [JSONValue]
  public init(text: String, toolCalls: [InferenceToolCall] = [], responsesOutput: [JSONValue] = [])
  {
    self.text = text
    self.toolCalls = toolCalls
    self.responsesOutput = responsesOutput
  }
}
public enum ModelInferenceError: LocalizedError, Equatable {
  case invalidProfile, insecureEndpoint, invalidChatGPTProfile, invalidCredential, transport,
    invalidResponse, incomplete, limitExceeded
  case http(Int)
  case provider(String)
  public var errorDescription: String? {
    switch self {
    case .invalidProfile: return "Enter a valid profile name, endpoint, and model."
    case .insecureEndpoint:
      return "Use HTTPS for remote model endpoints. HTTP is allowed only for exact loopback hosts."
    case .invalidChatGPTProfile:
      return "ChatGPT profiles require a selected account and the public OpenAI Responses endpoint."
    case .invalidCredential: return "The model credential is invalid."
    case .transport: return "The model connection failed. No automatic retry was attempted."
    case .invalidResponse: return "The model endpoint returned an invalid response."
    case .incomplete:
      return "Inference ended without successful completion. Partial output must not trigger tools."
    case .limitExceeded:
      return "The inference request or response exceeded the supported size limit."
    case .http(let status):
      return
        "The model endpoint rejected the request (HTTP \(status)). No automatic retry was attempted."
    case .provider(let code):
      switch code {
      case "subscription_sharing_usage_limit_exceeded":
        return
          "This ChatGPT account has reached its shared usage limit. No billing or account fallback was attempted."
      case "subscription_sharing_usage_unavailable":
        return
          "ChatGPT shared usage is unavailable for this account. No billing or account fallback was attempted."
      case "invalid_api_key", "authentication_error":
        return "The model endpoint rejected the credential."
      case "rate_limit_exceeded", "insufficient_quota":
        return "The model endpoint reported a usage or rate limit."
      case "model_not_found": return "The selected model is unavailable to this account."
      case "context_length_exceeded":
        return "The conversation exceeds the selected model's context limit."
      default: return "Inference failed at the model endpoint. No automatic retry was attempted."
      }
    }
  }
}
private final class InferenceRedirectPolicy: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  func urlSession(
    _ session: URLSession, task: URLSessionTask,
    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
    completionHandler: @escaping (URLRequest?) -> Void
  ) { completionHandler(nil) }
}
public final class ModelInference: @unchecked Sendable {
  private let configuration: URLSessionConfiguration
  private let tokenProvider: (@Sendable (ModelProfile) async throws -> String?)?
  public init(
    configuration: URLSessionConfiguration = .ephemeral,
    tokenProvider: (@Sendable (ModelProfile) async throws -> String?)? = nil
  ) {
    self.configuration = configuration.copy() as! URLSessionConfiguration
    self.tokenProvider = tokenProvider
  }
  private func token(for profile: ModelProfile) async throws -> String? {
    let value: String?
    if let tokenProvider {
      value = try await tokenProvider(profile)
    } else if profile.auth == .chatGPT {
      guard let account = profile.chatGPTAccountID else {
        throw ModelInferenceError.invalidChatGPTProfile
      }
      value = try await ChatGPTAuth.shared.accessToken(accountID: account)
    } else {
      value = ModelProfile.storedAPIKey(for: profile)
    }
    if let value {
      guard value.utf8.count <= 16384, !value.contains(where: { $0.isNewline || $0 == "\0" }) else {
        throw ModelInferenceError.invalidCredential
      }
    }
    return value?.isEmpty == true ? nil : value
  }
  private func session() -> URLSession {
    let config = configuration.copy() as! URLSessionConfiguration
    config.urlCache = nil
    config.httpCookieStorage = nil
    config.requestCachePolicy = .reloadIgnoringLocalCacheData
    config.timeoutIntervalForRequest = 90
    config.timeoutIntervalForResource = 300
    return URLSession(
      configuration: config, delegate: InferenceRedirectPolicy(), delegateQueue: nil)
  }
  private func request(
    profile: ModelProfile, path: String, body: Data? = nil, override: String? = nil
  ) async throws -> URLRequest {
    let base = try profile.validatedURL(requireModel: body != nil)
    var request = URLRequest(url: base.appendingPathComponent(path))
    request.httpMethod = body == nil ? "GET" : "POST"
    request.httpBody = body
    let credential: String?
    if profile.auth == .apiKey, let override {
      credential = override.isEmpty ? nil : override
    } else {
      credential = try await token(for: profile)
    }
    if let credential {
      guard credential.utf8.count <= 16384,
        !credential.contains(where: { $0.isNewline || $0 == "\0" })
      else { throw ModelInferenceError.invalidCredential }
      request.setValue("Bearer " + credential, forHTTPHeaderField: "Authorization")
    }
    request.setValue(
      body == nil ? "application/json" : "text/event-stream", forHTTPHeaderField: "Accept")
    if body != nil { request.setValue("application/json", forHTTPHeaderField: "Content-Type") }
    return request
  }
  public func models(profile: ModelProfile) async throws -> [String] {
    try await models(profile: profile, apiKeyOverride: nil)
  }
  public func models(profile: ModelProfile, apiKeyOverride: String?) async throws -> [String] {
    do {
      let request = try await request(profile: profile, path: "models", override: apiKeyOverride)
      let session = session()
      defer { session.invalidateAndCancel() }
      let (bytes, response) = try await session.bytes(for: request)
      guard let response = response as? HTTPURLResponse else {
        throw ModelInferenceError.invalidResponse
      }
      var data = Data()
      for try await byte in bytes {
        try Task.checkCancellation()
        guard data.count < 2 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
        data.append(byte)
      }
      guard (200..<300).contains(response.statusCode) else {
        throw Self.safeHTTPError(response.statusCode, data: data)
      }
      guard let payload = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        throw ModelInferenceError.invalidResponse
      }
      var values: [String] = []
      if profile.auth == .chatGPT {
        guard let models = payload["models"] as? [[String: Any]] else {
          throw ModelInferenceError.invalidResponse
        }
        values = models.filter { ($0["visibility"] as? String) == "list" }.compactMap {
          $0["slug"] as? String
        }
      } else if let models = payload["data"] as? [[String: Any]] {
        values = models.compactMap { $0["id"] as? String }
      } else if let models = payload["models"] as? [[String: Any]] {
        values = models.compactMap { ($0["slug"] ?? $0["id"]) as? String }
      } else {
        throw ModelInferenceError.invalidResponse
      }
      var seen = Set<String>()
      return values.filter { !$0.isEmpty && $0.utf8.count <= 256 && seen.insert($0).inserted }
    } catch { throw Self.clean(error) }
  }
  public func complete(
    profile: ModelProfile, messages: [InferenceMessage], tools: [InferenceTool] = [],
    onDelta: @escaping @Sendable (String) -> Void = { _ in }
  ) async throws -> InferenceReply {
    do {
      let body = try Self.body(profile: profile, messages: messages, tools: tools)
      let request = try await request(
        profile: profile, path: profile.api == .responses ? "responses" : "chat/completions",
        body: body)
      let session = session()
      defer { session.invalidateAndCancel() }
      let (bytes, response) = try await session.bytes(for: request)
      guard let response = response as? HTTPURLResponse else {
        throw ModelInferenceError.invalidResponse
      }
      if !(200..<300).contains(response.statusCode) {
        var data = Data()
        for try await byte in bytes {
          guard data.count < 65536 else { break }
          data.append(byte)
        }
        throw Self.safeHTTPError(response.statusCode, data: data)
      }
      guard
        response.value(forHTTPHeaderField: "Content-Type")?.lowercased().contains(
          "text/event-stream") == true
      else { throw ModelInferenceError.invalidResponse }
      var parser = InferenceSSEParser()
      var state = InferenceStreamState(api: profile.api)
      var total = 0
      for try await byte in bytes {
        try Task.checkCancellation()
        total += 1
        guard total <= 16 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
        if let event = try parser.feed(byte) {
          try state.consume(event, onDelta: onDelta)
          if state.completed { break }
        }
      }
      guard state.completed else { throw ModelInferenceError.incomplete }
      return try state.reply()
    } catch { throw Self.clean(error) }
  }
  private static func clean(_ error: Error) -> Error {
    if error is CancellationError || (error as? URLError)?.code == .cancelled {
      return CancellationError()
    }
    if let error = error as? ModelInferenceError { return error }
    if let error = error as? ChatGPTAuthError { return error }
    return ModelInferenceError.transport
  }
  static func safeCode(_ code: String?) -> String {
    let known: Set<String> = [
      "subscription_sharing_usage_limit_exceeded", "subscription_sharing_usage_unavailable",
      "invalid_api_key", "authentication_error", "rate_limit_exceeded", "insufficient_quota",
      "model_not_found", "context_length_exceeded",
    ]
    return code.flatMap { known.contains($0) ? $0 : nil } ?? "provider_error"
  }
  private static func safeHTTPError(_ status: Int, data: Data) -> ModelInferenceError {
    let payload = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    let code = safeCode((payload?["error"] as? [String: Any])?["code"] as? String)
    return code == "provider_error" ? .http(status) : .provider(code)
  }
  private static func body(
    profile: ModelProfile, messages: [InferenceMessage], tools: [InferenceTool]
  ) throws -> Data {
    _ = try profile.validatedURL()
    guard messages.count <= 512, tools.count <= 128 else { throw ModelInferenceError.limitExceeded }
    let roles: Set<String> = ["system", "developer", "user", "assistant", "tool"]
    guard messages.allSatisfy({ roles.contains($0.role) }) else {
      throw ModelInferenceError.invalidResponse
    }
    func object(_ value: JSONValue) throws -> Any {
      try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(value), options: .fragmentsAllowed)
    }
    var payload: [String: Any] = ["model": profile.model, "stream": true]
    if profile.api == .chatCompletions {
      payload[(profile.chatTokenParameter ?? .maxCompletionTokens).rawValue] = 8192
      payload["messages"] = messages.map { message -> [String: Any] in
        var value: [String: Any] = ["role": message.role, "content": message.content]
        if let id = message.toolCallID { value["tool_call_id"] = id }
        if !message.toolCalls.isEmpty {
          value["tool_calls"] = message.toolCalls.map {
            [
              "id": $0.id, "type": "function",
              "function": ["name": $0.name, "arguments": $0.arguments],
            ] as [String: Any]
          }
        }
        return value
      }
      if !tools.isEmpty {
        payload["tools"] = try tools.map { tool in
          [
            "type": "function",
            "function": [
              "name": tool.name, "description": tool.description,
              "parameters": try object(tool.parameters),
            ],
          ] as [String: Any]
        }
      }
    } else {
      payload["store"] = false
      payload["max_output_tokens"] = 8192
      payload["include"] = ["reasoning.encrypted_content"]
      var input: [[String: Any]] = []
      for message in messages {
        if message.role == "assistant", !message.responsesOutput.isEmpty {
          for item in message.responsesOutput {
            guard let item = try object(item) as? [String: Any] else {
              throw ModelInferenceError.invalidResponse
            }
            input.append(item)
          }
        } else if message.role == "tool" {
          guard let id = message.toolCallID, !id.isEmpty else {
            throw ModelInferenceError.invalidResponse
          }
          input.append(["type": "function_call_output", "call_id": id, "output": message.content])
        } else {
          if !message.content.isEmpty {
            input.append(["role": message.role, "content": message.content])
          }
          for call in message.toolCalls {
            input.append([
              "type": "function_call", "call_id": call.id, "name": call.name,
              "arguments": call.arguments,
            ])
          }
        }
      }
      payload["input"] = input
      if !tools.isEmpty {
        payload["tools"] = try tools.map { tool in
          [
            "type": "function", "name": tool.name, "description": tool.description,
            "parameters": try object(tool.parameters), "strict": false,
          ] as [String: Any]
        }
      }
    }
    let data = try JSONSerialization.data(withJSONObject: payload)
    guard data.count <= 8 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
    return data
  }
}

struct InferenceSSEParser {
  private var line = [UInt8]()
  private var data = [String]()
  private var eventBytes = 0
  mutating func feed(_ byte: UInt8) throws -> String? {
    if byte != 10 {
      guard line.count < 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
      line.append(byte)
      return nil
    }
    if line.last == 13 { line.removeLast() }
    guard let value = String(bytes: line, encoding: .utf8) else {
      throw ModelInferenceError.invalidResponse
    }
    line.removeAll(keepingCapacity: true)
    if value.isEmpty {
      guard !data.isEmpty else { return nil }
      let result = data.joined(separator: "\n")
      data.removeAll(keepingCapacity: true)
      eventBytes = 0
      return result
    }
    if value.hasPrefix("data:") {
      var item = String(value.dropFirst(5))
      if item.first == " " { item.removeFirst() }
      eventBytes += item.utf8.count + 1
      guard eventBytes <= 2 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
      data.append(item)
    }
    return nil
  }
}
private struct PartialInferenceCall {
  var id = ""
  var name = ""
  var arguments = ""
}
struct InferenceStreamState {
  let api: ModelAPI
  private(set) var completed = false
  private var text = ""
  private var textBytes = 0
  private var calls: [Int: PartialInferenceCall] = [:]
  private var itemIndexes: [String: Int] = [:]
  private var addedIndexes = Set<Int>()
  private var doneItems: [Int: JSONValue] = [:]
  private var canonicalOutput: [JSONValue]?
  private var chatFinished = false
  init(api: ModelAPI) { self.api = api }
  private mutating func append(_ delta: String, onDelta: @Sendable (String) -> Void) throws {
    textBytes += delta.utf8.count
    guard textBytes <= 2 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
    text += delta
    onDelta(delta)
  }
  private func providerFailure(_ event: [String: Any]) -> ModelInferenceError {
    let response = event["response"] as? [String: Any]
    let error = (response?["error"] as? [String: Any]) ?? (event["error"] as? [String: Any])
    return .provider(ModelInference.safeCode((error?["code"] ?? event["code"]) as? String))
  }
  private func jsonValue(_ value: Any) throws -> JSONValue {
    try JSONDecoder().decode(
      JSONValue.self,
      from: JSONSerialization.data(withJSONObject: value, options: .fragmentsAllowed))
  }
  private mutating func record(_ item: [String: Any], index: Int) throws {
    guard index >= 0, index < 128 else { throw ModelInferenceError.limitExceeded }
    if let itemID = item["id"] as? String { itemIndexes[itemID] = index }
    guard item["type"] as? String == "function_call" else { return }
    var call = calls[index] ?? PartialInferenceCall()
    if let id = item["call_id"] as? String { call.id = id }
    if let name = item["name"] as? String { call.name = name }
    if let arguments = item["arguments"] as? String { call.arguments = arguments }
    guard call.arguments.utf8.count <= 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
    calls[index] = call
  }
  mutating func consume(_ payload: String, onDelta: @Sendable (String) -> Void) throws {
    if payload == "[DONE]" {
      guard api == .chatCompletions, chatFinished else { throw ModelInferenceError.incomplete }
      completed = true
      return
    }
    guard let data = payload.data(using: .utf8),
      let event = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    else { throw ModelInferenceError.invalidResponse }
    if let error = event["error"], !(error is NSNull) { throw providerFailure(event) }
    if api == .chatCompletions {
      try consumeChat(event, onDelta: onDelta)
      return
    }
    let type = event["type"] as? String ?? ""
    switch type {
    case "response.output_text.delta", "response.refusal.delta":
      if let delta = event["delta"] as? String { try append(delta, onDelta: onDelta) }
    case "response.output_item.added":
      guard let index = event["output_index"] as? Int, let item = event["item"] as? [String: Any]
      else { throw ModelInferenceError.invalidResponse }
      addedIndexes.insert(index)
      try record(item, index: index)
    case "response.output_item.done":
      guard let index = event["output_index"] as? Int, let item = event["item"] as? [String: Any]
      else { throw ModelInferenceError.invalidResponse }
      try record(item, index: index)
      doneItems[index] = try jsonValue(item)
    case "response.function_call_arguments.delta":
      let index =
        (event["output_index"] as? Int) ?? (event["item_id"] as? String).flatMap { itemIndexes[$0] }
      guard let index, index >= 0, index < 128, let delta = event["delta"] as? String else {
        throw ModelInferenceError.invalidResponse
      }
      var call = calls[index] ?? PartialInferenceCall()
      call.arguments += delta
      guard call.arguments.utf8.count <= 1024 * 1024 else {
        throw ModelInferenceError.limitExceeded
      }
      calls[index] = call
    case "response.function_call_arguments.done":
      let index =
        (event["output_index"] as? Int) ?? (event["item_id"] as? String).flatMap { itemIndexes[$0] }
      guard let index, var call = calls[index], let arguments = event["arguments"] as? String else {
        throw ModelInferenceError.invalidResponse
      }
      guard arguments.utf8.count <= 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
      call.arguments = arguments
      calls[index] = call
    case "response.incomplete": throw ModelInferenceError.incomplete
    case "response.failed", "error": throw providerFailure(event)
    case "response.completed":
      guard let response = event["response"] as? [String: Any] else {
        throw ModelInferenceError.invalidResponse
      }
      if let status = response["status"] as? String, status != "completed" {
        throw ModelInferenceError.incomplete
      }
      if let error = response["error"], !(error is NSNull) { throw providerFailure(event) }
      if response["output"] != nil, !(response["output"] is [[String: Any]]) {
        throw ModelInferenceError.invalidResponse
      }
      if let output = response["output"] as? [[String: Any]] {
        guard output.count <= 128 else { throw ModelInferenceError.limitExceeded }
        canonicalOutput = try output.map { try jsonValue($0) }
        calls.removeAll()
        var finalText = ""
        for (index, item) in output.enumerated() {
          try record(item, index: index)
          if let content = item["content"] as? [[String: Any]] {
            for part in content {
              if let value = (part["text"] ?? part["refusal"]) as? String { finalText += value }
            }
          }
        }
        if !finalText.isEmpty {
          guard finalText.utf8.count <= 2 * 1024 * 1024 else {
            throw ModelInferenceError.limitExceeded
          }
          if text.isEmpty { onDelta(finalText) }
          text = finalText
        }
      } else {
        guard addedIndexes.isSubset(of: Set(doneItems.keys)) else {
          throw ModelInferenceError.incomplete
        }
        canonicalOutput = doneItems.keys.sorted().compactMap { doneItems[$0] }
      }
      completed = true
    default: break
    }
  }
  private mutating func consumeChat(_ event: [String: Any], onDelta: @Sendable (String) -> Void)
    throws
  {
    guard let choices = event["choices"] as? [[String: Any]] else {
      throw ModelInferenceError.invalidResponse
    }
    guard let choice = choices.first(where: { ($0["index"] as? Int ?? 0) == 0 }) else { return }
    if let delta = choice["delta"] as? [String: Any] {
      if let content = delta["content"] as? String { try append(content, onDelta: onDelta) }
      if let refusal = delta["refusal"] as? String { try append(refusal, onDelta: onDelta) }
      if let items = delta["tool_calls"] as? [[String: Any]] {
        for item in items {
          guard let index = item["index"] as? Int, index >= 0, index < 128 else {
            throw ModelInferenceError.invalidResponse
          }
          var call = calls[index] ?? PartialInferenceCall()
          if let id = item["id"] as? String {
            guard call.id.isEmpty || call.id == id else {
              throw ModelInferenceError.invalidResponse
            }
            call.id = id
          }
          if let function = item["function"] as? [String: Any] {
            if let name = function["name"] as? String { call.name += name }
            if let arguments = function["arguments"] as? String { call.arguments += arguments }
          }
          guard call.arguments.utf8.count <= 1024 * 1024 else {
            throw ModelInferenceError.limitExceeded
          }
          calls[index] = call
        }
      }
    }
    if let reason = choice["finish_reason"] as? String {
      guard ["stop", "tool_calls"].contains(reason) else { throw ModelInferenceError.incomplete }
      chatFinished = true
    }
  }
  func reply() throws -> InferenceReply {
    guard completed else { throw ModelInferenceError.incomplete }
    let ordered = calls.keys.sorted().compactMap { calls[$0] }
    var ids = Set<String>()
    var total = 0
    let tools = try ordered.map { call -> InferenceToolCall in
      guard !call.id.isEmpty, call.id.utf8.count <= 256, !call.name.isEmpty,
        call.name.utf8.count <= 128, ids.insert(call.id).inserted,
        let data = call.arguments.data(using: .utf8),
        (try? JSONSerialization.jsonObject(with: data)) is [String: Any]
      else { throw ModelInferenceError.invalidResponse }
      total += call.arguments.utf8.count
      guard total <= 4 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
      return InferenceToolCall(id: call.id, name: call.name, arguments: call.arguments)
    }
    let output = canonicalOutput ?? doneItems.keys.sorted().compactMap { doneItems[$0] }
    var resultText = text
    if resultText.isEmpty {
      for item in output {
        if case .array(let content)? = item["content"] {
          for part in content {
            if case .string(let value)? = part["text"] ?? part["refusal"] { resultText += value }
          }
        }
      }
    }
    guard resultText.utf8.count <= 2 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
    let encoded = try JSONEncoder().encode(output)
    guard encoded.count <= 8 * 1024 * 1024 else { throw ModelInferenceError.limitExceeded }
    return InferenceReply(text: resultText, toolCalls: tools, responsesOutput: output)
  }
}
