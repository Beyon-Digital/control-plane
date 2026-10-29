import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2
import Security

public struct NativeGRPCClient: Sendable {
  private let endpoint: URL
  private let realm: String
  private let token: String
  private let account: String?
  public init(address: String, httpBaseURL: URL, token: String) throws {
    guard let endpoint = URL(string: address),
      endpoint.host?.lowercased() == httpBaseURL.host?.lowercased(),
      endpoint.user == nil, endpoint.password == nil, endpoint.query == nil,
      endpoint.fragment == nil,
      endpoint.path.isEmpty || endpoint.path == "/", endpoint.scheme == httpBaseURL.scheme
    else { throw APIError.invalidAddress }
    guard
      endpoint.scheme == "https"
        || ["127.0.0.1", "localhost", "::1", "[::1]"].contains(endpoint.host ?? "")
    else { throw APIError.insecureAddress }
    self.endpoint = endpoint
    self.realm = httpBaseURL.absoluteString
    self.token = token
    let parts = httpBaseURL.path.split(separator: "/")
    self.account = parts.count == 2 && parts[0] == "accounts" ? String(parts[1]) : nil
  }
  private func transport() throws -> HTTP2ClientTransport.TransportServices {
    var security: HTTP2ClientTransport.TransportServices.TransportSecurity = .plaintext
    if endpoint.scheme == "https" {
      var tls = HTTP2ClientTransport.TransportServices.TLS.defaults()
      if let ca = DeviceIdentity.trustAnchor(host: realm) {
        tls.trustRoots = .certificates([
          .bytes(Array(SecCertificateCopyData(ca) as Data), format: .der)
        ])
      }
      if try DeviceIdentity.identity(host: realm) != nil {
        let realm = realm
        tls.identityProvider = {
          guard let identity = try DeviceIdentity.identity(host: realm) else {
            throw APIError.credentials("Device identity unavailable.")
          }
          return identity
        }
      }
      security = .tls(tls)
    }
    return try HTTP2ClientTransport.TransportServices(
      target: .dns(
        host: endpoint.host!, port: endpoint.port ?? (endpoint.scheme == "https" ? 443 : 9848)),
      transportSecurity: security)
  }
  private var metadata: Metadata {
    var result = Metadata()
    result.addString(token, forKey: "x-device-token")
    if let account { result.addString(account, forKey: "x-account-id") }
    return result
  }
  private static func mapped(_ error: Error) -> Error {
    guard let rpc = error as? RPCError else { return error }
    let code: Int
    switch rpc.code {
    case .unauthenticated: code = 401
    case .permissionDenied: code = 403
    case .resourceExhausted: code = 429
    case .failedPrecondition: code = 409
    default: code = 503
    }
    return APIError.server(code, rpc.message)
  }
  public func request(
    path: String, method: String, query: [String: String], body: [String: JSONValue]?
  ) async throws -> Data {
    do {
      var input = Controlplane_V1_APICallRequest()
      input.path = path
      input.method = method
      input.queryJson = String(decoding: try JSONEncoder().encode(query), as: UTF8.self)
      if let body {
        input.bodyJson = String(decoding: try JSONEncoder().encode(body), as: UTF8.self)
      }
      let metadata = metadata
      let result: Controlplane_V1_Document = try await withGRPCClient(transport: try transport()) {
        client in
        try await Controlplane_V1_HostService.Client(wrapping: client).call(
          input, metadata: metadata)
      }
      return Data(result.json.utf8)
    } catch { throw Self.mapped(error) }
  }
  public func events(sessionID: String, after: UInt64) -> AsyncThrowingStream<SessionEvent, Error> {
    AsyncThrowingStream(bufferingPolicy: .bufferingOldest(128)) { continuation in
      let task = Task {
        do {
          var input = Controlplane_V1_SubscribeRequest()
          input.sessionID = sessionID
          input.after = after
          let metadata = metadata
          try await withGRPCClient(transport: try transport()) { client in
            try await Controlplane_V1_SessionService.Client(wrapping: client).subscribe(
              input, metadata: metadata
            ) { response in
              for try await event in response.messages {
                guard event.sequence <= UInt64(Int64.max) else { throw APIError.invalidResponse }
                let item = SessionEvent(
                  sequence: Int64(event.sequence), session_id: event.sessionID, kind: event.kind,
                  payload: try JSONDecoder().decode(
                    JSONValue.self, from: Data(event.payloadJson.utf8)),
                  created_at: .number(Double(event.createdAt)))
                if case .dropped = continuation.yield(item) {
                  throw APIError.server(
                    429, "Event consumer is too slow; reconnect from the durable cursor.")
                }
              }
            }
          }
          continuation.finish()
        } catch { continuation.finish(throwing: Self.mapped(error)) }
      }
      continuation.onTermination = { _ in task.cancel() }
    }
  }
}
