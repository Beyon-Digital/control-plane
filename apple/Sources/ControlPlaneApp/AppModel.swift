import Foundation
import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif

struct SavedHost: Codable, Identifiable, Hashable {
  var id: String { address }
  var address: String
  var name: String
}

struct TimelineEntry: Identifiable {
  let event: SessionEvent
  var text: String
  var id: Int64 { event.sequence }
}

@MainActor final class AppModel: ObservableObject {
  @Published var savedHosts: [SavedHost] = []
  @Published var host: HostInfo?
  @Published var sessions: [AgentSession] = []
  @Published var selectedID: String? { didSet { if oldValue != selectedID { subscribe() } } }
  @Published var events: [SessionEvent] = []
  @Published var connection = "Disconnected"
  @Published var error: String?
  @Published var connecting = false
  @Published var currentAddress = ""
  @Published var terminalText = ""
  @Published var terminalConnected = false
  var client: APIClient?
  #if os(macOS)
    let localHost = LocalHostController()
    func startLocalHost() {
      let panel = NSOpenPanel()
      panel.canChooseDirectories = true
      panel.canChooseFiles = false
      panel.allowsMultipleSelection = false
      panel.message = "Choose the workspace directory this Mac’s coding agents may access."
      guard panel.runModal() == .OK, let root = panel.url else { return }
      perform {
        self.connecting = true
        defer { self.connecting = false }
        try await self.localHost.start(workspace: root, model: self)
      }
    }
  #endif
  private var socket: URLSessionWebSocketTask?
  private var terminalSocket: URLSessionWebSocketTask?
  private var eventTask: Task<Void, Never>?
  private var terminalTask: Task<Void, Never>?
  private var terminalSequence: Int64 = 0
  var selected: AgentSession? { sessions.first { $0.id == selectedID } }
  var timeline: [TimelineEntry] {
    var result: [TimelineEntry] = []
    for event in events {
      if event.kind == "message_delta", let last = result.last,
        last.event.kind == "message_delta",
        last.event.payload["item_id"] == event.payload["item_id"]
      {
        result[result.count - 1].text += event.payload.text
      } else {
        result.append(TimelineEntry(event: event, text: event.payload.text))
      }
    }
    return result
  }
  func approvalResolved(_ event: SessionEvent) -> Bool {
    guard let request = event.payload["request_id"] else { return true }
    return events.contains { $0.kind == "approval_resolved" && $0.payload["request_id"] == request }
  }

  init() {
    if let data = UserDefaults.standard.data(forKey: "hosts"),
      let values = try? JSONDecoder().decode([SavedHost].self, from: data)
    {
      savedHosts = values
    }
  }
  func perform(_ work: @escaping () async throws -> Void) {
    Task {
      do { try await work() } catch is CancellationError {} catch {
        self.error = error.localizedDescription
      }
    }
  }
  func connect(address: String, token: String) async throws {
    connecting = true
    defer { connecting = false }
    var candidate = try APIClient(
      address: address.trimmingCharacters(in: .whitespacesAndNewlines), token: token)
    if candidate.baseURL.scheme == "https",
      let certificate = DeviceIdentity.certificate(host: candidate.baseURL.absoluteString),
      certificate.expires_at > Int64(Date().timeIntervalSince1970),
      certificate.expires_at < Int64(Date().addingTimeInterval(7 * 86400).timeIntervalSince1970)
    {
      let renewal = try APIClient(
        address: candidate.baseURL.absoluteString, token: token, grpcAddress: "")
      _ = try await DeviceIdentity.enroll(using: renewal)
      candidate = try APIClient(address: candidate.baseURL.absoluteString, token: token)
    }
    let info: HostInfo = try await candidate.request("v1/host")
    let list: [AgentSession] = try await candidate.request("v1/sessions")
    try CredentialStore.save(token, host: candidate.baseURL.absoluteString)
    eventTask?.cancel()
    socket?.cancel(with: .goingAway, reason: nil)
    client = candidate
    host = info
    currentAddress = candidate.baseURL.absoluteString
    sessions = list
    connection = "Connected"
    await PushCoordinator.shared.sync(client: candidate, host: currentAddress)
    savedHosts.removeAll { $0.address == currentAddress }
    savedHosts.append(SavedHost(address: currentAddress, name: info.name))
    UserDefaults.standard.set(try JSONEncoder().encode(savedHosts), forKey: "hosts")
    selectedID = list.first?.id
    if selectedID != nil { subscribe() }
  }
  func reconnect(_ saved: SavedHost) {
    guard let token = CredentialStore.load(host: saved.address) else {
      error = "This host has no saved device credential. Pair again."
      return
    }
    perform { try await self.connect(address: saved.address, token: token) }
  }
  func refresh() async throws {
    guard let client else { return }
    sessions = try await client.request("v1/sessions")
  }
  func create(
    provider: String, workspace: String, title: String, trusted: Bool,
    executionTarget: JSONValue = .object(["kind": .string("native")])
  ) async throws {
    guard let client else { return }
    let effectiveTitle =
      title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
      ? "\(provider.capitalized) · \((workspace as NSString).lastPathComponent)" : title
    let session: AgentSession = try await client.request(
      "v1/sessions", method: "POST",
      body: [
        "provider": .string(provider), "workspace": .string(workspace),
        "title": .string(effectiveTitle), "trusted": .bool(trusted),
        "execution_target": executionTarget, "request_id": .string(UUID().uuidString),
      ])
    try await refresh()
    selectedID = session.id
  }
  func send(_ text: String) async throws {
    guard let client, let selectedID else { return }
    try await client.mutate(
      "v1/sessions/\(selectedID)/prompt",
      body: ["text": .string(text), "request_id": .string(UUID().uuidString)])
    try await refresh()
  }
  func action(_ name: String) async throws {
    guard let client, let selectedID else { return }
    try await client.mutate("v1/sessions/\(selectedID)/\(name)")
    try await refresh()
  }
  func decision(_ request: JSONValue, decision: String) async throws {
    guard let client, let selectedID else { return }
    try await client.mutate(
      "v1/sessions/\(selectedID)/decision",
      body: ["request_id": request, "decision": .string(decision)])
  }
  func stopAll() async throws {
    guard let client else { return }
    try await client.mutate("v1/emergency-stop")
    try await refresh()
  }
  func subscribe() {
    eventTask?.cancel()
    socket?.cancel(with: .goingAway, reason: nil)
    closeTerminal()
    events = []
    guard let client, let id = selectedID else { return }
    eventTask = Task {
      var delay: UInt64 = 1
      while !Task.isCancelled && selectedID == id {
        do {
          let history: [SessionEvent] = try await client.request(
            "v1/sessions/\(id)/events", query: ["after": String(events.last?.sequence ?? 0)])
          append(history)
          if let grpc = client.grpc {
            connection = "Connected · gRPC"
            delay = 1
            for try await event in grpc.events(
              sessionID: id, after: UInt64(max(0, events.last?.sequence ?? 0)))
            { append([event]) }
            throw APIError.server(503, "Event stream ended")
          }
          let ws = client.eventSocket(sessionID: id, after: events.last?.sequence ?? 0)
          socket = ws
          ws.resume()
          connection = "Connected"
          delay = 1
          while !Task.isCancelled {
            let message = try await ws.receive()
            let data: Data
            switch message {
            case .data(let value): data = value
            case .string(let value): data = Data(value.utf8)
            @unknown default: continue
            }
            if let event = try? JSONDecoder().decode(SessionEvent.self, from: data) {
              append([event])
            } else if let value = try? JSONDecoder().decode(JSONValue.self, from: data),
              value["error"] != nil
            {
              throw APIError.server(409, value["error"]!.text)
            }
          }
        } catch {
          guard !Task.isCancelled else { return }
          if case APIError.server(409, let message) = error, message.contains("history_gap") {
            events = []
            try? await refresh()
            continue
          }
          connection = "Reconnecting · \(error.localizedDescription)"
          try? await Task.sleep(nanoseconds: delay * 1_000_000_000)
          delay = min(delay * 2, 30)
        }
      }
    }
  }
  private func append(_ incoming: [SessionEvent]) {
    let cursor = events.last?.sequence ?? 0
    events.append(
      contentsOf: incoming.filter { $0.sequence > cursor }.sorted { $0.sequence < $1.sequence })
    if events.count > 3000 { events.removeFirst(events.count - 3000) }
    if incoming.contains(where: { $0.kind == "state" }) { perform { try await self.refresh() } }
  }
  func openTerminal() {
    guard terminalTask == nil, let client, let id = selectedID else { return }
    var parts = URLComponents(
      url: client.baseURL.appendingPathComponent("v1/terminal"), resolvingAgainstBaseURL: false)!
    parts.scheme = client.baseURL.scheme == "https" ? "wss" : "ws"
    parts.queryItems = [
      URLQueryItem(name: "session_id", value: id),
      URLQueryItem(name: "after", value: String(terminalSequence)),
    ]
    var request = URLRequest(url: parts.url!)
    request.setValue(
      CredentialStore.load(host: currentAddress), forHTTPHeaderField: "X-Device-Token")
    let ws = URLSession.shared.webSocketTask(with: request)
    terminalSocket = ws
    ws.resume()
    terminalConnected = true
    terminalTask = Task {
      defer {
        terminalConnected = false
        terminalTask = nil
      }
      do {
        while !Task.isCancelled {
          let message = try await ws.receive()
          let data: Data
          switch message {
          case .data(let d): data = d
          case .string(let s): data = Data(s.utf8)
          @unknown default: continue
          }
          let value = try JSONDecoder().decode(JSONValue.self, from: data)
          if let base64 = value["data"]?.text, let decoded = Data(base64Encoded: base64) {
            terminalText += String(decoding: decoded, as: UTF8.self)
          }
          if case .number(let n) = value["sequence"] { terminalSequence = Int64(n) }
          if value["type"]?.text == "exit" {
            terminalText += "\n[Process exited]\n"
            return
          }
          if let error = value["error"] { self.error = error.text }
          if terminalText.count > 200_000 { terminalText = String(terminalText.suffix(150_000)) }
        }
      } catch { if !Task.isCancelled { self.error = error.localizedDescription } }
    }
  }
  func terminalInput(_ text: String) async throws {
    guard let terminalSocket else { return }
    let value: [String: JSONValue] = [
      "type": .string("input"), "data": .string(Data(text.utf8).base64EncodedString()),
    ]
    try await terminalSocket.send(.data(JSONEncoder().encode(value)))
  }
  func closeTerminal() {
    terminalTask?.cancel()
    terminalSocket?.cancel(with: .goingAway, reason: nil)
    terminalTask = nil
    terminalSocket = nil
    terminalConnected = false
    terminalText = ""
    terminalSequence = 0
  }
}
