import Security
import XCTest

#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif

private final class AuthMemoryStore: ChatGPTCredentialStorage, @unchecked Sendable {
  private let lock = NSLock()
  private var items: [String: Data] = [:]
  func read(_ key: String) throws -> Data? {
    lock.lock()
    defer { lock.unlock() }
    return items[key]
  }
  func write(_ value: Data, key: String) throws {
    lock.lock()
    defer { lock.unlock() }
    items[key] = value
  }
}
private final class AuthStubProtocol: URLProtocol, @unchecked Sendable {
  static var handler: ((URLRequest) throws -> (Int, Data))?
  private let lock = NSLock()
  private var cancelled = false
  override class func canInit(with request: URLRequest) -> Bool { true }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    DispatchQueue.global().async {
      do {
        guard let handler = Self.handler else { throw URLError(.badServerResponse) }
        let (code, data) = try handler(self.request)
        self.lock.lock()
        let cancelled = self.cancelled
        self.lock.unlock()
        if cancelled { return }
        self.client?.urlProtocol(
          self,
          didReceive: HTTPURLResponse(
            url: self.request.url!, statusCode: code, httpVersion: nil, headerFields: nil)!,
          cacheStoragePolicy: .notAllowed)
        self.client?.urlProtocol(self, didLoad: data)
        self.client?.urlProtocolDidFinishLoading(self)
      } catch { self.client?.urlProtocol(self, didFailWithError: error) }
    }
  }
  override func stopLoading() {
    lock.lock()
    cancelled = true
    lock.unlock()
  }
}
private final class AuthCounter: @unchecked Sendable {
  private let lock = NSLock()
  private var count = 0
  func increment() {
    lock.lock()
    count += 1
    lock.unlock()
  }
  var value: Int {
    lock.lock()
    defer { lock.unlock() }
    return count
  }
}
private struct AuthSigningFixture {
  let privateKey: SecKey
  let jwks: ChatGPTJWKS
  init() throws {
    guard
      let key = SecKeyCreateRandomKey(
        [kSecAttrKeyType: kSecAttrKeyTypeRSA, kSecAttrKeySizeInBits: 2048] as CFDictionary, nil),
      let publicKey = SecKeyCopyPublicKey(key),
      let exported = SecKeyCopyExternalRepresentation(publicKey, nil) as Data?
    else { throw URLError(.cannotDecodeContentData) }
    privateKey = key
    let bytes = [UInt8](exported)
    var offset = 0
    func length() throws -> Int {
      guard offset < bytes.count else { throw URLError(.cannotDecodeContentData) }
      let first = Int(bytes[offset])
      offset += 1
      if first < 128 { return first }
      let count = first & 127
      guard count > 0, count <= 4, offset + count <= bytes.count else {
        throw URLError(.cannotDecodeContentData)
      }
      var value = 0
      for _ in 0..<count {
        value = (value << 8) | Int(bytes[offset])
        offset += 1
      }
      return value
    }
    guard bytes[offset] == 0x30 else { throw URLError(.cannotDecodeContentData) }
    offset += 1
    _ = try length()
    func integer() throws -> Data {
      guard bytes[offset] == 2 else { throw URLError(.cannotDecodeContentData) }
      offset += 1
      let count = try length()
      guard offset + count <= bytes.count else { throw URLError(.cannotDecodeContentData) }
      var result = Data(bytes[offset..<offset + count])
      offset += count
      while result.count > 1, result.first == 0 { result.removeFirst() }
      return result
    }
    let n = try integer()
    let e = try integer()
    jwks = ChatGPTJWKS(keys: [
      ChatGPTJWK(
        kty: "RSA", kid: "fixture-key", n: ChatGPTCrypto.base64URL(n),
        e: ChatGPTCrypto.base64URL(e), alg: "RS256", use: "sig")
    ])
  }
  func token(
    clientID: String = "oaiapp_fixture", nonce: String = "nonce", subject: String = "subject",
    issuer: String = "https://auth.openai.com",
    expiry: TimeInterval = Date().addingTimeInterval(3600).timeIntervalSince1970
  ) throws -> String {
    let header = try JSONSerialization.data(withJSONObject: ["alg": "RS256", "kid": "fixture-key"])
    let payload = try JSONSerialization.data(withJSONObject: [
      "iss": issuer, "sub": subject, "aud": clientID, "exp": expiry,
      "iat": Date().timeIntervalSince1970, "nonce": nonce, "email": "person@example.com",
    ])
    let message = ChatGPTCrypto.base64URL(header) + "." + ChatGPTCrypto.base64URL(payload)
    guard
      let signature = SecKeyCreateSignature(
        privateKey, .rsaSignatureMessagePKCS1v15SHA256, Data(message.utf8) as CFData, nil) as Data?
    else { throw URLError(.cannotDecodeContentData) }
    return message + "." + ChatGPTCrypto.base64URL(signature)
  }
}
final class ChatGPTAuthTests: XCTestCase {
  private let discovery = Data(
    #"{"issuer":"https://auth.openai.com","authorization_endpoint":"https://auth.openai.com/api/accounts/authorize","token_endpoint":"https://auth.openai.com/api/accounts/oauth/token","revocation_endpoint":"https://auth.openai.com/api/accounts/oauth/revoke","jwks_uri":"https://auth.openai.com/.well-known/jwks.json"}"#
      .utf8)
  private func auth(_ store: AuthMemoryStore) -> ChatGPTAuth {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [AuthStubProtocol.self]
    return ChatGPTAuth(storage: store, session: URLSession(configuration: config))
  }
  private func seed(_ store: AuthMemoryStore) throws {
    let account = ChatGPTAccount(
      id: "oaiapp_fixture", label: "Saved workspace", subject: "subject",
      email: "person@example.com", canInfer: true)
    let value = ChatGPTCredential(
      account: account, hostID: "urn:uuid:fixture-host", accessToken: "old-access",
      refreshToken: "old-refresh", idToken: nil, expiresAt: Date().addingTimeInterval(-1),
      earliestRefreshAt: nil, scopes: ["chatgpt.tokens.use.direct", "resource.invoke"])
    try store.write(JSONEncoder().encode(value), key: "account:oaiapp_fixture")
    try store.write(JSONEncoder().encode([account.id]), key: "registrations")
    try store.write(Data(value.hostID.utf8), key: "host-id")
  }
  private func fields(_ request: ChatGPTAuthorizationRequest) -> [String: String] {
    Dictionary(
      uniqueKeysWithValues: URLComponents(
        url: request.authorizationURL, resolvingAgainstBaseURL: false)!.queryItems!.map {
          ($0.name, $0.value ?? "")
        })
  }
  private func callback(
    _ request: ChatGPTAuthorizationRequest, clientID: String = "oaiapp_fixture",
    state: String? = nil
  ) -> URL {
    let values = fields(request)
    var url = URLComponents(string: values["redirect_uri"]!)!
    url.queryItems = [
      URLQueryItem(name: "code", value: "fixture-code"),
      URLQueryItem(name: "state", value: state ?? values["state"]!),
      URLQueryItem(name: "client_id", value: clientID),
    ]
    return url.url!
  }
  func testPKCEPublishedVectorAndSignatureIdentityChecks() throws {
    XCTAssertEqual(
      ChatGPTCrypto.challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
      "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
    let fixture = try AuthSigningFixture()
    let jwt = try fixture.token()
    XCTAssertEqual(
      try ChatGPTCrypto.verify(jwt, jwks: fixture.jwks, clientID: "oaiapp_fixture", nonce: "nonce")
        .subject, "subject")
    XCTAssertThrowsError(
      try ChatGPTCrypto.verify(jwt, jwks: fixture.jwks, clientID: "wrong-client", nonce: "nonce"))
    XCTAssertThrowsError(
      try ChatGPTCrypto.verify(
        jwt, jwks: fixture.jwks, clientID: "oaiapp_fixture", nonce: "wrong-nonce"))
    XCTAssertThrowsError(
      try ChatGPTCrypto.verify(
        fixture.token(issuer: "https://evil.invalid"), jwks: fixture.jwks,
        clientID: "oaiapp_fixture", nonce: "nonce"))
    XCTAssertThrowsError(
      try ChatGPTCrypto.verify(
        fixture.token(expiry: Date().addingTimeInterval(-1).timeIntervalSince1970),
        jwks: fixture.jwks, clientID: "oaiapp_fixture", nonce: "nonce"))
    var parts = jwt.split(separator: ".").map(String.init)
    var signature = try ChatGPTCrypto.decode(parts[2])
    signature[0] ^= 1
    parts[2] = ChatGPTCrypto.base64URL(signature)
    XCTAssertThrowsError(
      try ChatGPTCrypto.verify(
        parts.joined(separator: "."), jwks: fixture.jwks, clientID: "oaiapp_fixture", nonce: "nonce"
      ))
  }
  func testDynamicRegistrationValidatesStateAndRetainsStableHost() async throws {
    let store = AuthMemoryStore()
    let client = auth(store)
    let discovery = self.discovery
    let exchanges = AuthCounter()
    AuthStubProtocol.handler = { request in
      XCTAssertEqual(request.url?.host, "auth.openai.com")
      if request.url?.path == "/api/accounts/oauth/token" { exchanges.increment() }
      return (200, discovery)
    }
    let first = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!)
    let second = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54322/auth/callback")!)
    XCTAssertEqual(fields(first)["client_id"], "dynamic_agent_client")
    XCTAssertEqual(fields(first)["agent_name_hint"], "Control Plane")
    XCTAssertEqual(fields(first)["ext_agent_host_id"], fields(second)["ext_agent_host_id"])
    XCTAssertNotEqual(fields(first)["state"], fields(second)["state"])
    do {
      _ = try await client.completeSignIn(
        attemptID: first.id, callbackURL: callback(first, state: "wrong"))
      XCTFail("State must be checked before token exchange")
    } catch {}
    XCTAssertEqual(exchanges.value, 0)
    do {
      _ = try await client.prepareSignIn(
        redirectURI: URL(string: "http://localhost:54321/auth/callback")!)
      XCTFail("localhost must be rejected")
    } catch {}
  }
  func testSuccessfulRegistrationUsesValidatedIdentityAndGrantedScopes() async throws {
    let store = AuthMemoryStore()
    let client = auth(store)
    let fixture = try AuthSigningFixture()
    let discovery = self.discovery
    AuthStubProtocol.handler = { _ in (200, discovery) }
    let pending = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!)
    let nonce = fields(pending)["nonce"]!
    let jwt = try fixture.token(nonce: nonce)
    let jwks = try JSONEncoder().encode(fixture.jwks)
    AuthStubProtocol.handler = { request in
      switch request.url!.path {
      case "/.well-known/jwks.json": return (200, jwks)
      case "/api/accounts/oauth/token":
        return (
          200,
          try JSONSerialization.data(withJSONObject: [
            "access_token": "approved-access", "refresh_token": "approved-refresh", "id_token": jwt,
            "token_type": "Bearer", "expires_in": 3600,
            "scope":
              "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
          ])
        )
      default: return (200, discovery)
      }
    }
    let account = try await client.completeSignIn(
      attemptID: pending.id, callbackURL: callback(pending))
    XCTAssertEqual(account.subject, "subject")
    XCTAssertEqual(account.email, "person@example.com")
    XCTAssertTrue(account.canInfer)
    let token = try await client.accessToken(accountID: account.id)
    XCTAssertEqual(token, "approved-access")
    let reauth = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54322/auth/callback")!, accountID: account.id)
    XCTAssertEqual(fields(reauth)["client_id"], account.id)
    XCTAssertNil(fields(reauth)["agent_name_hint"])
    XCTAssertEqual(fields(reauth)["id_token_hint"], jwt)
    do {
      _ = try await client.completeSignIn(
        attemptID: reauth.id, callbackURL: callback(reauth, clientID: "different-client"))
      XCTFail("Client registration must remain bound")
    } catch {}
  }
  func testIdentityChangeCannotReplaceExistingRegistration() async throws {
    let store = AuthMemoryStore()
    try seed(store)
    let client = auth(store)
    let discovery = self.discovery
    let fixture = try AuthSigningFixture()
    AuthStubProtocol.handler = { _ in (200, discovery) }
    let pending = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!, accountID: "oaiapp_fixture"
    )
    let jwt = try fixture.token(nonce: fields(pending)["nonce"]!, subject: "different-subject")
    let jwks = try JSONEncoder().encode(fixture.jwks)
    AuthStubProtocol.handler = { request in
      if request.url!.path == "/.well-known/jwks.json" { return (200, jwks) }
      return (
        200,
        try JSONSerialization.data(withJSONObject: [
          "access_token": "wrong-account-access", "refresh_token": "wrong-refresh", "id_token": jwt,
          "token_type": "Bearer", "expires_in": 3600,
          "scope": "resource.invoke chatgpt.tokens.use.direct",
        ])
      )
    }
    do {
      _ = try await client.completeSignIn(attemptID: pending.id, callbackURL: callback(pending))
      XCTFail("Wrong subject must be rejected")
    } catch {}
    let record = try JSONDecoder().decode(
      ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
    XCTAssertEqual(record.accessToken, "old-access")
    XCTAssertEqual(record.account.subject, "subject")
  }
  func testRefreshIsSingleFlightAndAtomicallyRotatesCredentialRecord() async throws {
    let store = AuthMemoryStore()
    try seed(store)
    let client = auth(store)
    let discovery = self.discovery
    let count = AuthCounter()
    AuthStubProtocol.handler = { request in
      if request.url!.path == "/.well-known/openid-configuration" { return (200, discovery) }
      count.increment()
      Thread.sleep(forTimeInterval: 0.1)
      return (
        200,
        Data(
          #"{"access_token":"new-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":3600,"scope":"resource.invoke chatgpt.tokens.use.direct"}"#
            .utf8)
      )
    }
    let values = try await withThrowingTaskGroup(of: String.self) { group in
      for _ in 0..<10 {
        group.addTask { try await client.accessToken(accountID: "oaiapp_fixture") }
      }
      var values = [String]()
      for try await value in group { values.append(value) }
      return values
    }
    XCTAssertEqual(values, Array(repeating: "new-access", count: 10))
    XCTAssertEqual(count.value, 1)
    let record = try JSONDecoder().decode(
      ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
    XCTAssertEqual(record.refreshToken, "rotated-refresh")
    XCTAssertEqual(record.accessToken, "new-access")
  }
  func testSignOutDrainsRotationRevokesNewestTokenAndCannotRepopulateTokens() async throws {
    let store = AuthMemoryStore()
    try seed(store)
    let client = auth(store)
    let discovery = self.discovery
    let started = expectation(description: "refresh started")
    AuthStubProtocol.handler = { request in
      switch request.url!.path {
      case "/.well-known/openid-configuration": return (200, discovery)
      case "/api/accounts/oauth/revoke":
        let cleared = try JSONDecoder().decode(
          ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
        XCTAssertNil(cleared.accessToken)
        XCTAssertNil(cleared.refreshToken)
        XCTAssertTrue(Self.body(request).contains("token=late-refresh"))
        return (200, Data())
      default:
        started.fulfill()
        Thread.sleep(forTimeInterval: 0.2)
        return (
          200,
          Data(
            #"{"access_token":"late-access","refresh_token":"late-refresh","token_type":"Bearer","expires_in":3600,"scope":"resource.invoke chatgpt.tokens.use.direct"}"#
              .utf8)
        )
      }
    }
    let refresh = Task { try await client.accessToken(accountID: "oaiapp_fixture") }
    await fulfillment(of: [started], timeout: 2)
    try await client.signOut(accountID: "oaiapp_fixture")
    do {
      _ = try await refresh.value
      XCTFail("Cancelled refresh must not return tokens")
    } catch {}
    let record = try JSONDecoder().decode(
      ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
    XCTAssertNil(record.accessToken)
    XCTAssertNil(record.refreshToken)
    XCTAssertNil(record.idToken)
    XCTAssertEqual(record.account.subject, "subject")
    XCTAssertFalse(record.account.canInfer)
    do {
      _ = try await client.accessToken(accountID: "oaiapp_fixture")
      XCTFail("Signed-out credentials must not be used")
    } catch {}
  }
  func testUnconfirmedRemoteRevocationStillClearsLocalCredentials() async throws {
    let store = AuthMemoryStore()
    try seed(store)
    let client = auth(store)
    let discovery = self.discovery
    AuthStubProtocol.handler = { request in
      if request.url!.path == "/.well-known/openid-configuration" { return (200, discovery) }
      return (503, Data())
    }
    do {
      try await client.signOut(accountID: "oaiapp_fixture")
      XCTFail("Revocation must be reported unconfirmed")
    } catch ChatGPTAuthError.revocationUnconfirmed {} catch {
      XCTFail("Unexpected error: \(error)")
    }
    let record = try JSONDecoder().decode(
      ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
    XCTAssertNil(record.refreshToken)
    XCTAssertNil(record.accessToken)
    let accounts = try await client.accounts()
    XCTAssertEqual(accounts.count, 1)
    XCTAssertFalse(accounts[0].canInfer)
  }
  func testFailedCodeExchangeRetainsIssuedRegistrationForFreshAuthorization() async throws {
    let store = AuthMemoryStore()
    let client = auth(store)
    let discovery = self.discovery
    AuthStubProtocol.handler = { request in
      request.url!.path == "/.well-known/openid-configuration" ? (200, discovery) : (400, Data())
    }
    let first = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!)
    do {
      _ = try await client.completeSignIn(attemptID: first.id, callbackURL: callback(first))
      XCTFail("Code exchange should fail")
    } catch {}
    let retry = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54322/auth/callback")!)
    XCTAssertEqual(fields(retry)["client_id"], "oaiapp_fixture")
    XCTAssertNil(fields(retry)["agent_name_hint"])
    let accounts = try await client.accounts()
    XCTAssertTrue(accounts.isEmpty, "An unverified registration cannot become an account")
  }
  func testCancelledExchangeCannotReplaceCredentialsAfterAwaitingJWKS() async throws {
    let store = AuthMemoryStore()
    try seed(store)
    let client = auth(store)
    let discovery = self.discovery
    let fixture = try AuthSigningFixture()
    AuthStubProtocol.handler = { _ in (200, discovery) }
    let pending = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!, accountID: "oaiapp_fixture"
    )
    let jwt = try fixture.token(nonce: fields(pending)["nonce"]!)
    let jwks = try JSONEncoder().encode(fixture.jwks)
    let started = expectation(description: "JWKS pending")
    AuthStubProtocol.handler = { request in
      if request.url!.path == "/.well-known/jwks.json" {
        started.fulfill()
        Thread.sleep(forTimeInterval: 0.2)
        return (200, jwks)
      }
      return (
        200,
        try JSONSerialization.data(withJSONObject: [
          "access_token": "cancelled-access", "refresh_token": "cancelled-refresh", "id_token": jwt,
          "token_type": "Bearer", "expires_in": 3600,
          "scope": "resource.invoke chatgpt.tokens.use.direct",
        ])
      )
    }
    let callbackURL = callback(pending)
    let exchange = Task {
      try await client.completeSignIn(attemptID: pending.id, callbackURL: callbackURL)
    }
    await fulfillment(of: [started], timeout: 2)
    await client.cancelSignIn(attemptID: pending.id)
    do {
      _ = try await exchange.value
      XCTFail("Cancelled exchange cannot persist tokens")
    } catch {}
    let record = try JSONDecoder().decode(
      ChatGPTCredential.self, from: store.read("account:oaiapp_fixture")!)
    XCTAssertEqual(record.accessToken, "old-access")
  }
  func testValidIdentityWithoutDirectGrantCannotInfer() async throws {
    let store = AuthMemoryStore()
    let client = auth(store)
    let fixture = try AuthSigningFixture()
    let discovery = self.discovery
    AuthStubProtocol.handler = { _ in (200, discovery) }
    let pending = try await client.prepareSignIn(
      redirectURI: URL(string: "http://127.0.0.1:54321/auth/callback")!)
    let jwt = try fixture.token(nonce: fields(pending)["nonce"]!)
    let jwks = try JSONEncoder().encode(fixture.jwks)
    AuthStubProtocol.handler = { request in
      if request.url?.path == "/.well-known/jwks.json" { return (200, jwks) }
      return (
        200,
        try JSONSerialization.data(withJSONObject: [
          "access_token": "identity-only", "refresh_token": "fixture-refresh", "id_token": jwt,
          "token_type": "Bearer", "expires_in": 3600,
          "scope": "openid profile email resource.invoke",
        ])
      )
    }
    let account = try await client.completeSignIn(
      attemptID: pending.id, callbackURL: callback(pending))
    XCTAssertFalse(account.canInfer)
    do {
      _ = try await client.accessToken(accountID: account.id)
      XCTFail("Identity alone cannot authorize inference")
    } catch ChatGPTAuthError.reauthorizationRequired {}
  }
  private static func body(_ request: URLRequest) -> String {
    if let data = request.httpBody { return String(decoding: data, as: UTF8.self) }
    guard let stream = request.httpBodyStream else { return "" }
    stream.open()
    defer { stream.close() }
    var data = Data()
    var bytes = [UInt8](repeating: 0, count: 4096)
    while stream.hasBytesAvailable {
      let n = stream.read(&bytes, maxLength: bytes.count)
      if n <= 0 { break }
      data.append(contentsOf: bytes.prefix(n))
    }
    return String(decoding: data, as: UTF8.self)
  }
}
