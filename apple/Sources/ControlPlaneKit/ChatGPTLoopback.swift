import Foundation
import Network

/// One-use OAuth callback receiver. It binds IPv4 loopback only and ignores invalid-state probes.
public final class ChatGPTLoopback: @unchecked Sendable {
  private let queue = DispatchQueue(label: "dev.controlplane.chatgpt.loopback")
  private var listener: NWListener?
  private var connections: [ObjectIdentifier: NWConnection] = [:]
  private var redirect: URL?
  private var expectedState: String?
  private var ready: CheckedContinuation<URL, Error>?
  private var callback: CheckedContinuation<URL, Error>?
  private var result: Result<URL, Error>?
  private var started = false

  public init() {}

  public func start() async throws -> URL {
    try await withTaskCancellationHandler {
      try await withCheckedThrowingContinuation { continuation in
        queue.async { [self] in
          guard !self.started, self.result == nil else {
            continuation.resume(
              throwing: OrchestratorError.invalid("This sign-in listener is already closed."))
            return
          }
          self.started = true
          self.ready = continuation
          do {
            let parameters = NWParameters.tcp
            parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
            let listener = try NWListener(using: parameters, on: .any)
            self.listener = listener
            listener.stateUpdateHandler = { [weak self] state in
              guard let self else { return }
              switch state {
              case .ready:
                guard let port = listener.port,
                  let url = URL(string: "http://127.0.0.1:\(port.rawValue)/auth/callback")
                else {
                  self.finish(
                    .failure(
                      OrchestratorError.invalid("Could not start the ChatGPT sign-in callback.")))
                  return
                }
                self.redirect = url
                self.ready?.resume(returning: url)
                self.ready = nil
              case .failed:
                self.finish(
                  .failure(
                    OrchestratorError.invalid("Could not start the local ChatGPT sign-in callback.")
                  ))
              default: break
              }
            }
            listener.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
            listener.start(queue: self.queue)
            self.queue.asyncAfter(deadline: .now() + 180) { [weak self] in
              self?.finish(
                .failure(
                  OrchestratorError.invalid(
                    "ChatGPT sign-in timed out. Start a new sign-in attempt.")))
            }
          } catch {
            self.finish(
              .failure(
                OrchestratorError.invalid("Could not bind the local ChatGPT sign-in callback.")))
          }
        }
      }
    } onCancel: {
      self.stop()
    }
  }

  public func setExpectedState(_ state: String) async {
    await withCheckedContinuation { continuation in
      queue.async {
        self.expectedState = state
        continuation.resume()
      }
    }
  }

  public func waitForCallback() async throws -> URL {
    try await withTaskCancellationHandler {
      try await withCheckedThrowingContinuation { continuation in
        queue.async {
          if let result = self.result {
            continuation.resume(with: result)
          } else if self.callback != nil {
            continuation.resume(
              throwing: OrchestratorError.invalid("A sign-in callback is already pending."))
          } else {
            self.callback = continuation
          }
        }
      }
    } onCancel: {
      self.stop()
    }
  }

  public func stop() { queue.async { self.finish(.failure(CancellationError())) } }

  private func accept(_ connection: NWConnection) {
    guard result == nil, connections.count < 8 else {
      connection.cancel()
      return
    }
    let key = ObjectIdentifier(connection)
    connections[key] = connection
    connection.start(queue: queue)
    queue.asyncAfter(deadline: .now() + 5) { [weak self, weak connection] in
      connection?.cancel()
      self?.connections.removeValue(forKey: key)
    }
    receive(connection, accumulated: Data())
  }

  private func receive(_ connection: NWConnection, accumulated: Data) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) {
      [weak self] bytes, _, complete, error in
      guard let self else {
        connection.cancel()
        return
      }
      var data = accumulated
      if let bytes { data.append(bytes) }
      guard data.count <= 16 * 1024 else {
        self.respond(connection, status: 431)
        return
      }
      if data.range(of: Data("\r\n\r\n".utf8)) != nil {
        guard let line = String(data: data, encoding: .utf8)?.components(separatedBy: "\r\n").first,
          let redirect = self.redirect
        else {
          self.respond(connection, status: 400)
          return
        }
        let parts = line.split(separator: " ", omittingEmptySubsequences: false)
        let hosts = String(decoding: data, as: UTF8.self).components(separatedBy: "\r\n")
          .dropFirst().compactMap { header -> String? in
            let pair = header.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
            guard pair.count == 2, pair[0].lowercased() == "host" else { return nil }
            return pair[1].trimmingCharacters(in: .whitespaces)
          }
        guard parts.count == 3, parts[0] == "GET", parts[2] == "HTTP/1.1" || parts[2] == "HTTP/1.0",
          hosts.count == 1, hosts[0] == "127.0.0.1:\(redirect.port!)",
          parts[1].hasPrefix("/auth/callback?"),
          let url = URL(string: "http://127.0.0.1:\(redirect.port!)/" + parts[1].dropFirst()),
          url.path == "/auth/callback", url.fragment == nil,
          let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems,
          items.filter({ $0.name == "state" }).count == 1,
          let state = items.first(where: { $0.name == "state" })?.value,
          let expected = self.expectedState, Self.same(state, expected)
        else {
          self.respond(connection, status: 400)
          return
        }
        self.respond(connection, status: 200) { self.finish(.success(url)) }
      } else if complete || error != nil {
        connection.cancel()
        self.connections.removeValue(forKey: ObjectIdentifier(connection))
      } else {
        self.receive(connection, accumulated: data)
      }
    }
  }

  private static func same(_ lhs: String, _ rhs: String) -> Bool {
    let a = Array(lhs.utf8)
    let b = Array(rhs.utf8)
    guard a.count == b.count else { return false }
    return zip(a, b).reduce(UInt8(0)) { $0 | ($1.0 ^ $1.1) } == 0
  }

  private func respond(_ connection: NWConnection, status: Int, completion: (() -> Void)? = nil) {
    let body =
      status == 200
      ? "ChatGPT sign-in received. You can return to Control Plane." : "Invalid sign-in callback."
    let response =
      "HTTP/1.1 \(status) \(status == 200 ? "OK" : "Bad Request")\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: \(body.utf8.count)\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\n\(body)"
    connection.send(
      content: Data(response.utf8),
      completion: .contentProcessed { [weak self] _ in
        connection.cancel()
        self?.connections.removeValue(forKey: ObjectIdentifier(connection))
        completion?()
      })
  }

  private func finish(_ outcome: Result<URL, Error>) {
    guard result == nil else { return }
    result = outcome
    listener?.stateUpdateHandler = nil
    listener?.newConnectionHandler = nil
    listener?.cancel()
    listener = nil
    for connection in connections.values { connection.cancel() }
    connections.removeAll()
    if let ready {
      ready.resume(with: outcome)
      self.ready = nil
    }
    callback?.resume(with: outcome)
    callback = nil
  }
}
