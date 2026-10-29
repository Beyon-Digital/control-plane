import CryptoKit
import Foundation
import Security

public struct ChatGPTAccount: Codable, Identifiable, Hashable, Sendable {
  public let id: String
  public let label: String
  public let subject: String
  public let email: String?
  public let canInfer: Bool
}
public struct ChatGPTAuthorizationRequest: Sendable {
  public let id: UUID
  public let authorizationURL: URL
}
public enum ChatGPTAuthError: LocalizedError {
  case invalid(String)
  case cancelled, reauthorizationRequired, revocationUnconfirmed
  public var errorDescription: String? {
    switch self {
    case .invalid(let text): return text
    case .cancelled: return "ChatGPT sign-in was cancelled."
    case .reauthorizationRequired: return "Sign in to this ChatGPT account again to renew access."
    case .revocationUnconfirmed:
      return
        "Signed out on this device. Remote revocation was not confirmed; disconnect the app in ChatGPT Settings to end remaining access."
    }
  }
}
struct ChatGPTJWK: Codable {
  let kty: String
  let kid: String
  let n: String?
  let e: String?
  let alg: String?
  let use: String?
}
struct ChatGPTJWKS: Codable { let keys: [ChatGPTJWK] }
struct ChatGPTIdentity: Equatable {
  let subject: String
  let email: String?
}
enum ChatGPTCrypto {
  static func base64URL(_ data: Data) -> String {
    data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(
      of: "/", with: "_"
    ).replacingOccurrences(of: "=", with: "")
  }
  static func decode(_ text: String) throws -> Data {
    guard !text.isEmpty,
      text.utf8.allSatisfy({
        (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || $0 == 45
          || $0 == 95
      })
    else { throw ChatGPTAuthError.invalid("Invalid token encoding.") }
    let padded =
      text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
      + String(repeating: "=", count: (4 - text.count % 4) % 4)
    guard let data = Data(base64Encoded: padded) else {
      throw ChatGPTAuthError.invalid("Invalid token encoding.")
    }
    return data
  }
  static func random(_ count: Int = 32) throws -> String {
    var bytes = [UInt8](repeating: 0, count: count)
    guard SecRandomCopyBytes(kSecRandomDefault, count, &bytes) == errSecSuccess else {
      throw ChatGPTAuthError.invalid("Secure randomness unavailable.")
    }
    return base64URL(Data(bytes))
  }
  static func challenge(_ verifier: String) -> String {
    base64URL(Data(SHA256.hash(data: Data(verifier.utf8))))
  }
  static func equal(_ a: String, _ b: String) -> Bool {
    let x = Array(a.utf8)
    let y = Array(b.utf8)
    guard x.count == y.count else { return false }
    var difference: UInt8 = 0
    for i in x.indices { difference |= x[i] ^ y[i] }
    return difference == 0
  }
  static func verify(
    _ jwt: String, jwks: ChatGPTJWKS, clientID: String, nonce: String?, now: Date = Date()
  ) throws -> ChatGPTIdentity {
    let pieces = jwt.split(separator: ".", omittingEmptySubsequences: false).map(String.init)
    guard pieces.count == 3, jwt.utf8.count <= 65536 else {
      throw ChatGPTAuthError.invalid("Invalid ID token.")
    }
    let header = try JSONSerialization.jsonObject(with: decode(pieces[0])) as? [String: Any]
    guard header?["alg"] as? String == "RS256", let kid = header?["kid"] as? String, !kid.isEmpty,
      header?["crit"] == nil, header?["jku"] == nil, header?["x5u"] == nil
    else { throw ChatGPTAuthError.invalid("Unsupported ID-token signature.") }
    let matches = jwks.keys.filter {
      $0.kid == kid && $0.kty == "RSA" && ($0.alg == nil || $0.alg == "RS256")
        && ($0.use == nil || $0.use == "sig")
    }
    guard matches.count == 1, let n = matches[0].n, let e = matches[0].e else {
      throw ChatGPTAuthError.invalid("ID-token signing key unavailable.")
    }
    let modulus = try decode(n)
    let exponent = try decode(e)
    guard modulus.count >= 256, modulus.count <= 1024, !exponent.isEmpty, exponent.count <= 8 else {
      throw ChatGPTAuthError.invalid("Unsupported RSA key size.")
    }
    guard modulus.first != 0, modulus.count > 256 || (modulus.first! & 0x80 != 0) else {
      throw ChatGPTAuthError.invalid("RSA signing keys must have at least 2048 bits.")
    }
    func length(_ count: Int) -> Data {
      if count < 128 { return Data([UInt8(count)]) }
      var bytes = [UInt8]()
      var remaining = count
      while remaining > 0 {
        bytes.insert(UInt8(remaining & 255), at: 0)
        remaining >>= 8
      }
      return Data([0x80 | UInt8(bytes.count)] + bytes)
    }
    func integer(_ bytes: Data) -> Data {
      let value = bytes.first.map { $0 & 0x80 != 0 } == true ? Data([0]) + bytes : bytes
      return Data([2]) + length(value.count) + value
    }
    let body = integer(modulus) + integer(exponent)
    let der = Data([0x30]) + length(body.count) + body
    guard
      let key = SecKeyCreateWithData(
        der as CFData,
        [kSecAttrKeyType: kSecAttrKeyTypeRSA, kSecAttrKeyClass: kSecAttrKeyClassPublic]
          as CFDictionary, nil),
      SecKeyIsAlgorithmSupported(key, .verify, .rsaSignatureMessagePKCS1v15SHA256),
      SecKeyVerifySignature(
        key, .rsaSignatureMessagePKCS1v15SHA256, Data((pieces[0] + "." + pieces[1]).utf8) as CFData,
        try decode(pieces[2]) as CFData, nil)
    else { throw ChatGPTAuthError.invalid("ID-token signature validation failed.") }
    guard let claims = try JSONSerialization.jsonObject(with: decode(pieces[1])) as? [String: Any],
      claims["iss"] as? String == "https://auth.openai.com", let subject = claims["sub"] as? String,
      !subject.isEmpty, let expiry = claims["exp"] as? Double, expiry > now.timeIntervalSince1970
    else {
      throw ChatGPTAuthError.invalid("ID token is expired or has an invalid issuer or identity.")
    }
    let audience = (claims["aud"] as? [String]) ?? (claims["aud"] as? String).map { [$0] } ?? []
    guard audience.contains(clientID), audience.count <= 1 || claims["azp"] as? String == clientID,
      claims["azp"] == nil || claims["azp"] as? String == clientID
    else { throw ChatGPTAuthError.invalid("ID token belongs to a different client.") }
    if let nonce {
      guard let received = claims["nonce"] as? String, equal(received, nonce) else {
        throw ChatGPTAuthError.invalid("ID-token nonce did not match this sign-in.")
      }
    }
    if let notBefore = claims["nbf"] as? Double, notBefore > now.timeIntervalSince1970 + 30 {
      throw ChatGPTAuthError.invalid("ID token is not yet valid.")
    }
    if let issued = claims["iat"] as? Double, issued > now.timeIntervalSince1970 + 30 {
      throw ChatGPTAuthError.invalid("ID-token issue time is invalid.")
    }
    return ChatGPTIdentity(subject: subject, email: claims["email"] as? String)
  }
}
protocol ChatGPTCredentialStorage: Sendable {
  func read(_ key: String) throws -> Data?
  func write(_ value: Data, key: String) throws
}
struct ChatGPTKeychain: ChatGPTCredentialStorage {
  private let service = "dev.controlplane.chatgpt"
  func read(_ key: String) throws -> Data? {
    var result: CFTypeRef?
    let status = SecItemCopyMatching(
      [
        kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
        kSecAttrAccount as String: key, kSecReturnData as String: true,
        kSecMatchLimit as String: kSecMatchLimitOne,
      ] as CFDictionary, &result)
    if status == errSecItemNotFound { return nil }
    guard status == errSecSuccess, let data = result as? Data else {
      throw ChatGPTAuthError.invalid("Could not read protected ChatGPT credentials (\(status)).")
    }
    return data
  }
  func write(_ value: Data, key: String) throws {
    let query: [String: Any] = [
      kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
      kSecAttrAccount as String: key,
    ]
    var status = SecItemUpdate(
      query as CFDictionary, [kSecValueData as String: value] as CFDictionary)
    if status == errSecItemNotFound {
      var item = query
      item[kSecValueData as String] = value
      item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
      status = SecItemAdd(item as CFDictionary, nil)
    }
    guard status == errSecSuccess else {
      throw ChatGPTAuthError.invalid("Could not save protected ChatGPT credentials (\(status)).")
    }
  }
}
