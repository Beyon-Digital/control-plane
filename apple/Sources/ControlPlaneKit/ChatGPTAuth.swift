import Foundation

private final class ChatGPTNoRedirect: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  func urlSession(
    _ session: URLSession, task: URLSessionTask,
    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
    completionHandler: @escaping (URLRequest?) -> Void
  ) { completionHandler(nil) }
}
struct ChatGPTCredential: Codable {
  var account: ChatGPTAccount
  var hostID: String
  var accessToken: String?
  var refreshToken: String?
  var idToken: String?
  var expiresAt: Date
  var earliestRefreshAt: Date?
  var scopes: [String]
}
private struct ChatGPTDiscovery: Decodable {
  let issuer: String
  let authorization_endpoint: String
  let token_endpoint: String
  let revocation_endpoint: String
  let jwks_uri: String
}
private struct ChatGPTTokenResponse: Decodable {
  let access_token: String
  let refresh_token: String?
  let id_token: String?
  let token_type: String
  let expires_in: Double
  let scope: String
  let earliest_refresh_at: Double?
}
private struct ChatGPTPending {
  let redirect: URL
  let selected: ChatGPTCredential?
  let state: String
  let nonce: String
  let verifier: String
  let hostID: String
  let created: Date
  let generation: UInt64
  let requestedClientID: String
}
public actor ChatGPTAuth {
  public static let shared = ChatGPTAuth()
  public static let resource = "https://api.openai.com/v1"
  private let storage: any ChatGPTCredentialStorage
  private let session: URLSession
  private var pending: [UUID: ChatGPTPending] = [:]
  private var exchanging: Set<UUID> = []
  private var generations: [String: UInt64] = [:]
  private var authEpoch: UInt64 = 0
  private var refreshes: [String: Task<String, Error>] = [:]
  private var revocationTokens: [String: String] = [:]
  private var signedOut: Set<String> = []
  private var signingOut: Set<String> = []
  private var discoveryCache: ChatGPTDiscovery?
  public init() {
    storage = ChatGPTKeychain()
    let config = URLSessionConfiguration.ephemeral
    config.urlCache = nil
    config.httpCookieStorage = nil
    config.requestCachePolicy = .reloadIgnoringLocalCacheData
    config.timeoutIntervalForRequest = 20
    config.timeoutIntervalForResource = 30
    session = URLSession(configuration: config, delegate: ChatGPTNoRedirect(), delegateQueue: nil)
  }
  init(storage: any ChatGPTCredentialStorage, session: URLSession) {
    self.storage = storage
    self.session = session
  }
  private func credential(_ id: String) throws -> ChatGPTCredential? {
    guard id != "dynamic_agent_client", !id.isEmpty else {
      throw ChatGPTAuthError.invalid("Invalid ChatGPT registration.")
    }
    guard let bytes = try storage.read("account:" + id) else { return nil }
    let value = try JSONDecoder().decode(ChatGPTCredential.self, from: bytes)
    guard value.account.id == id else {
      throw ChatGPTAuthError.invalid("ChatGPT credential identity mismatch.")
    }
    return value
  }
  private func save(_ record: ChatGPTCredential) throws {
    try storage.write(JSONEncoder().encode(record), key: "account:" + record.account.id)
    var ids =
      try storage.read("registrations").map { try JSONDecoder().decode([String].self, from: $0) }
      ?? []
    if !ids.contains(record.account.id) {
      ids.append(record.account.id)
      try storage.write(JSONEncoder().encode(ids), key: "registrations")
    }
  }
  public func accounts() throws -> [ChatGPTAccount] {
    let ids =
      try storage.read("registrations").map { try JSONDecoder().decode([String].self, from: $0) }
      ?? []
    return try ids.compactMap { try credential($0)?.account }.map { account in
      if signedOut.contains(account.id) {
        return ChatGPTAccount(
          id: account.id, label: account.label, subject: account.subject, email: account.email,
          canInfer: false)
      }
      return account
    }
  }
  private func hostID() throws -> String {
    if let data = try storage.read("host-id"), let value = String(data: data, encoding: .utf8),
      value.hasPrefix("urn:uuid:"), value.count > 9
    {
      return value
    }
    if let data = try storage.read("registrations"),
      let ids = try? JSONDecoder().decode([String].self, from: data), !ids.isEmpty
    {
      throw ChatGPTAuthError.invalid(
        "This device's ChatGPT host identity is missing. Repair protected credentials before reconnecting."
      )
    }
    let value = "urn:uuid:" + UUID().uuidString.lowercased()
    try storage.write(Data(value.utf8), key: "host-id")
    return value
  }
  private static func fixedAuthURL(_ text: String) throws -> URL {
    guard let url = URL(string: text), url.scheme == "https", url.host == "auth.openai.com",
      url.port == nil, url.user == nil, url.password == nil, url.fragment == nil, url.query == nil
    else { throw ChatGPTAuthError.invalid("OpenAI authentication endpoint is not trusted.") }
    return url
  }
  private func data(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
    let expected = try Self.fixedAuthURL(request.url!.absoluteString)
    let (stream, response) = try await session.bytes(for: request)
    guard let http = response as? HTTPURLResponse, http.url == expected else {
      throw ChatGPTAuthError.invalid("Invalid authentication response.")
    }
    var bytes = Data()
    for try await byte in stream {
      try Task.checkCancellation()
      guard bytes.count < 1024 * 1024 else {
        throw ChatGPTAuthError.invalid("Authentication response exceeded its size limit.")
      }
      bytes.append(byte)
    }
    return (bytes, http)
  }
  private func discovery() async throws -> ChatGPTDiscovery {
    if let value = discoveryCache { return value }
    let (bytes, http) = try await data(
      URLRequest(url: Self.fixedAuthURL("https://auth.openai.com/.well-known/openid-configuration"))
    )
    guard http.statusCode == 200 else {
      throw ChatGPTAuthError.invalid("OpenAI identity discovery is unavailable.")
    }
    let value = try JSONDecoder().decode(ChatGPTDiscovery.self, from: bytes)
    guard value.issuer == "https://auth.openai.com",
      value.authorization_endpoint == "https://auth.openai.com/api/accounts/authorize",
      value.token_endpoint == "https://auth.openai.com/api/accounts/oauth/token"
    else { throw ChatGPTAuthError.invalid("Unexpected OpenAI identity configuration.") }
    _ = try Self.fixedAuthURL(value.jwks_uri)
    _ = try Self.fixedAuthURL(value.revocation_endpoint)
    discoveryCache = value
    return value
  }
  private func keys() async throws -> ChatGPTJWKS {
    let config = try await discovery()
    let (bytes, http) = try await data(URLRequest(url: Self.fixedAuthURL(config.jwks_uri)))
    guard http.statusCode == 200 else {
      throw ChatGPTAuthError.invalid("OpenAI identity signing keys are unavailable.")
    }
    return try JSONDecoder().decode(ChatGPTJWKS.self, from: bytes)
  }
  static func form(_ values: [String: String]) -> Data {
    var permitted = CharacterSet.alphanumerics
    permitted.insert(charactersIn: "-._~")
    return Data(
      values.sorted { $0.key < $1.key }.map { key, value in
        key.addingPercentEncoding(withAllowedCharacters: permitted)! + "="
          + value.addingPercentEncoding(withAllowedCharacters: permitted)!
      }.joined(separator: "&").utf8)
  }
  private func post(_ endpoint: String, fields: [String: String]) async throws -> (
    Data, HTTPURLResponse
  ) {
    var request = URLRequest(url: try Self.fixedAuthURL(endpoint))
    request.httpMethod = "POST"
    request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
    request.httpBody = Self.form(fields)
    return try await data(request)
  }
  public func prepareSignIn(redirectURI: URL, accountID: String? = nil) async throws
    -> ChatGPTAuthorizationRequest
  {
    guard redirectURI.scheme == "http", redirectURI.host == "127.0.0.1",
      let port = redirectURI.port, (1024...65535).contains(port),
      redirectURI.path == "/auth/callback", redirectURI.query == nil, redirectURI.fragment == nil,
      redirectURI.user == nil, redirectURI.password == nil
    else {
      throw ChatGPTAuthError.invalid(
        "ChatGPT requires an exact HTTP loopback callback at 127.0.0.1/auth/callback.")
    }
    let selected = try accountID.map { id -> ChatGPTCredential in
      guard !signingOut.contains(id), let record = try credential(id) else {
        throw ChatGPTAuthError.reauthorizationRequired
      }
      return record
    }
    let epoch = authEpoch
    let generation = selected.map { generations[$0.account.id, default: 0] } ?? 0
    let config = try await discovery()
    try Task.checkCancellation()
    guard epoch == authEpoch,
      selected.map({
        !signingOut.contains($0.account.id) && generations[$0.account.id, default: 0] == generation
      }) ?? true
    else { throw ChatGPTAuthError.cancelled }
    let verifier = try ChatGPTCrypto.random()
    let state = try ChatGPTCrypto.random()
    let nonce = try ChatGPTCrypto.random()
    let host = try hostID()
    let id = UUID()
    if let selected {
      guard selected.hostID == host else {
        throw ChatGPTAuthError.invalid(
          "This ChatGPT registration belongs to a different host identity.")
      }
    }
    let staged = try storage.read("incomplete-registration").flatMap {
      String(data: $0, encoding: .utf8)
    }
    let requestedID =
      selected?.account.id ?? (staged?.isEmpty == false ? staged! : "dynamic_agent_client")
    var fields = [
      "client_id": requestedID, "ext_agent_host_id": host, "response_type": "code",
      "redirect_uri": redirectURI.absoluteString,
      "scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
      "resource": Self.resource, "state": state, "nonce": nonce, "code_challenge_method": "S256",
      "code_challenge": ChatGPTCrypto.challenge(verifier),
    ]
    if let selected {
      if let hint = selected.idToken { fields["id_token_hint"] = hint }
      if let email = selected.account.email { fields["login_hint"] = email }
    } else if requestedID == "dynamic_agent_client" {
      fields["agent_name_hint"] = "Control Plane"
    }
    var url = URLComponents(string: config.authorization_endpoint)!
    url.queryItems = fields.sorted { $0.key < $1.key }.map {
      URLQueryItem(name: $0.key, value: $0.value)
    }
    pending[id] = ChatGPTPending(
      redirect: redirectURI, selected: selected, state: state, nonce: nonce, verifier: verifier,
      hostID: host, created: Date(), generation: generation, requestedClientID: requestedID)
    return ChatGPTAuthorizationRequest(id: id, authorizationURL: url.url!)
  }
  public func cancelSignIn(attemptID: UUID) { pending.removeValue(forKey: attemptID) }
  public func completeSignIn(attemptID: UUID, callbackURL: URL) async throws -> ChatGPTAccount {
    guard let attempt = pending[attemptID], Date().timeIntervalSince(attempt.created) < 300,
      exchanging.insert(attemptID).inserted
    else { throw ChatGPTAuthError.cancelled }
    defer {
      pending.removeValue(forKey: attemptID)
      exchanging.remove(attemptID)
    }
    guard var callback = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false) else {
      throw ChatGPTAuthError.invalid("Invalid sign-in callback.")
    }
    let items = callback.queryItems ?? []
    callback.query = nil
    guard callback.url == attempt.redirect, Set(items.map(\.name)).count == items.count else {
      throw ChatGPTAuthError.invalid("Sign-in callback did not match the pending request.")
    }
    let values = Dictionary(uniqueKeysWithValues: items.map { ($0.name, $0.value ?? "") })
    guard let state = values["state"], ChatGPTCrypto.equal(state, attempt.state) else {
      throw ChatGPTAuthError.invalid("Sign-in state did not match.")
    }
    if values["error"] != nil {
      throw ChatGPTAuthError.invalid(
        "ChatGPT authorization was declined or could not be completed.")
    }
    let clientID =
      values["client_id"]
      ?? (attempt.requestedClientID != "dynamic_agent_client" ? attempt.requestedClientID : nil)
    guard let clientID, !clientID.isEmpty, clientID != "dynamic_agent_client",
      clientID.utf8.count <= 512, let code = values["code"], !code.isEmpty, code.utf8.count <= 8192
    else {
      throw ChatGPTAuthError.invalid(
        "ChatGPT registration did not return a code and issued client ID.")
    }
    if attempt.requestedClientID != "dynamic_agent_client" {
      guard clientID == attempt.requestedClientID else {
        throw ChatGPTAuthError.invalid("ChatGPT returned a different client registration.")
      }
    } else {
      try storage.write(Data(clientID.utf8), key: "incomplete-registration")
    }
    if let selected = attempt.selected {
      guard clientID == selected.account.id, generations[clientID, default: 0] == attempt.generation
      else { throw ChatGPTAuthError.cancelled }
    }
    let generation = generations[clientID, default: 0]
    guard !signingOut.contains(clientID) else { throw ChatGPTAuthError.cancelled }
    let config = try await discovery()
    let (bytes, http) = try await post(
      config.token_endpoint,
      fields: [
        "grant_type": "authorization_code", "client_id": clientID, "code": code,
        "code_verifier": attempt.verifier, "redirect_uri": attempt.redirect.absoluteString,
        "resource": Self.resource,
      ])
    guard http.statusCode == 200 else {
      throw ChatGPTAuthError.invalid(
        "ChatGPT code exchange failed. Start a fresh sign-in; this code was discarded.")
    }
    let response = try JSONDecoder().decode(ChatGPTTokenResponse.self, from: bytes)
    guard let idToken = response.id_token else {
      throw ChatGPTAuthError.invalid("ChatGPT did not return an ID token.")
    }
    let jwks = try await keys()
    let identity = try ChatGPTCrypto.verify(
      idToken, jwks: jwks, clientID: clientID, nonce: attempt.nonce)
    if let selected = attempt.selected {
      guard identity.subject == selected.account.subject else {
        throw ChatGPTAuthError.invalid("Reauthorization returned a different ChatGPT identity.")
      }
    }
    if let existing = try credential(clientID) {
      guard existing.account.subject == identity.subject else {
        throw ChatGPTAuthError.invalid("ChatGPT registration identity changed.")
      }
    }
    try Task.checkCancellation()
    guard pending[attemptID] != nil, !signingOut.contains(clientID),
      generations[clientID, default: 0] == generation
    else { throw ChatGPTAuthError.cancelled }
    let scopes = response.scope.split(separator: " ").map(String.init)
    let account = ChatGPTAccount(
      id: clientID,
      label: attempt.selected?.account.label
        ?? "\(identity.email ?? "ChatGPT account") · \(clientID.suffix(8))",
      subject: identity.subject, email: identity.email,
      canInfer: scopes.contains("chatgpt.tokens.use.direct") && scopes.contains("resource.invoke"))
    let record = try tokenRecord(
      response, account: account, hostID: attempt.hostID, retainedID: idToken, retainedRefresh: nil)
    generations[clientID, default: 0] &+= 1
    refreshes[clientID]?.cancel()
    try save(record)
    signedOut.remove(clientID)
    if try storage.read("incomplete-registration").flatMap({ String(data: $0, encoding: .utf8) })
      == clientID
    {
      try storage.write(Data(), key: "incomplete-registration")
    }
    return account
  }
  private func tokenRecord(
    _ response: ChatGPTTokenResponse, account: ChatGPTAccount, hostID: String, retainedID: String?,
    retainedRefresh: String?
  ) throws -> ChatGPTCredential {
    guard response.token_type.lowercased() == "bearer", !response.access_token.isEmpty,
      response.access_token.utf8.count <= 65536, response.expires_in.isFinite,
      response.expires_in > 0, response.expires_in <= 86400
    else { throw ChatGPTAuthError.invalid("Invalid ChatGPT token response.") }
    let refresh = response.refresh_token ?? retainedRefresh
    if let refresh, refresh.isEmpty || refresh.utf8.count > 65536 {
      throw ChatGPTAuthError.invalid("Invalid ChatGPT refresh token.")
    }
    return ChatGPTCredential(
      account: account, hostID: hostID, accessToken: response.access_token, refreshToken: refresh,
      idToken: response.id_token ?? retainedID,
      expiresAt: Date().addingTimeInterval(response.expires_in),
      earliestRefreshAt: response.earliest_refresh_at.map { Date(timeIntervalSince1970: $0) },
      scopes: response.scope.split(separator: " ").map(String.init))
  }
  public func accessToken(accountID: String) async throws -> String {
    guard !signedOut.contains(accountID), !signingOut.contains(accountID),
      let record = try credential(accountID), record.account.canInfer,
      let token = record.accessToken
    else { throw ChatGPTAuthError.reauthorizationRequired }
    if record.expiresAt.timeIntervalSinceNow > 60 { return token }
    if let earliest = record.earliestRefreshAt, earliest > Date() {
      if record.expiresAt > Date() { return token }
      throw ChatGPTAuthError.reauthorizationRequired
    }
    if let task = refreshes[accountID] { return try await task.value }
    let generation = generations[accountID, default: 0]
    let task = Task { try await self.refresh(accountID, generation: generation) }
    refreshes[accountID] = task
    defer { refreshes.removeValue(forKey: accountID) }
    return try await task.value
  }
  private func refresh(_ id: String, generation: UInt64) async throws -> String {
    guard let old = try credential(id), let refresh = old.refreshToken, !signedOut.contains(id),
      !signingOut.contains(id)
    else { throw ChatGPTAuthError.reauthorizationRequired }
    let config = try await discovery()
    let (bytes, http) = try await post(
      config.token_endpoint,
      fields: [
        "grant_type": "refresh_token", "client_id": id, "refresh_token": refresh,
        "resource": Self.resource,
      ])
    guard http.statusCode == 200 else { throw ChatGPTAuthError.reauthorizationRequired }
    let response = try JSONDecoder().decode(ChatGPTTokenResponse.self, from: bytes)
    guard let replacement = response.refresh_token, !replacement.isEmpty else {
      throw ChatGPTAuthError.invalid("Refresh did not return a replacement refresh token.")
    }
    if let idToken = response.id_token {
      let jwks = try await keys()
      let identity = try ChatGPTCrypto.verify(idToken, jwks: jwks, clientID: id, nonce: nil)
      guard identity.subject == old.account.subject else {
        throw ChatGPTAuthError.invalid("Refreshed ChatGPT identity changed.")
      }
    }
    if signingOut.contains(id) {
      _ = try tokenRecord(
        response, account: old.account, hostID: old.hostID, retainedID: old.idToken,
        retainedRefresh: old.refreshToken)
      revocationTokens[id] = replacement
      throw ChatGPTAuthError.cancelled
    }
    try Task.checkCancellation()
    guard generations[id, default: 0] == generation, !signedOut.contains(id),
      !signingOut.contains(id)
    else { throw ChatGPTAuthError.cancelled }
    let scopes = response.scope.split(separator: " ").map(String.init)
    let account = ChatGPTAccount(
      id: id, label: old.account.label, subject: old.account.subject, email: old.account.email,
      canInfer: scopes.contains("chatgpt.tokens.use.direct") && scopes.contains("resource.invoke"))
    let record = try tokenRecord(
      response, account: account, hostID: old.hostID, retainedID: old.idToken,
      retainedRefresh: old.refreshToken)
    try save(record)
    guard account.canInfer else { throw ChatGPTAuthError.reauthorizationRequired }
    return response.access_token
  }
  public func signOut(accountID: String) async throws {
    guard var record = try credential(accountID) else { return }
    guard !signingOut.contains(accountID) else {
      throw ChatGPTAuthError.invalid("This account is already signing out.")
    }
    let previousRefresh = record.refreshToken
    let inFlight = refreshes[accountID]
    signingOut.insert(accountID)
    signedOut.insert(accountID)
    generations[accountID, default: 0] &+= 1
    authEpoch &+= 1
    pending.removeAll()
    defer { signingOut.remove(accountID) }
    // Persist local logout before any network wait, including process termination.
    record.accessToken = nil
    record.refreshToken = nil
    record.idToken = nil
    record.expiresAt = .distantPast
    record.scopes = []
    record.earliestRefreshAt = nil
    record.account = ChatGPTAccount(
      id: record.account.id, label: record.account.label, subject: record.account.subject,
      email: record.account.email, canInfer: false)
    try save(record)
    // Drain an existing rotation so revocation uses the newest issued token.
    if let inFlight { _ = try? await inFlight.value }
    let renewable = revocationTokens.removeValue(forKey: accountID) ?? previousRefresh
    var confirmed = renewable == nil
    if let refresh = renewable {
      for attempt in 0..<3 {
        do {
          let config = try await discovery()
          let (_, http) = try await post(
            config.revocation_endpoint,
            fields: ["token": refresh, "token_type_hint": "refresh_token", "client_id": accountID])
          if http.statusCode == 200 {
            confirmed = true
            break
          }
          if http.statusCode < 500 { break }
        } catch {}
        if attempt < 2 {
          do { try await Task.sleep(nanoseconds: UInt64(attempt + 1) * 300_000_000) } catch {
            break
          }
        }
      }
    }
    if !confirmed { throw ChatGPTAuthError.revocationUnconfirmed }
  }
}
