import CryptoKit
import Foundation
import Security

public struct DeviceCertificateInfo: Codable, Sendable {
  public let device_id: String
  public let certificate_pem: String
  public let ca_pem: String
  public let fingerprint: String
  public let expires_at: Int64
}
public enum DeviceIdentity {
  private static func label(_ host: String) -> String {
    "dev.controlplane.device."
      + SHA256.hash(data: Data(host.utf8)).map { String(format: "%02x", $0) }.joined()
  }
  private static func key(_ host: String) throws -> SecKey {
    let tag = Data(label(host).utf8)
    let query: [String: Any] = [
      kSecClass as String: kSecClassKey, kSecAttrApplicationTag as String: tag,
      kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom, kSecReturnRef as String: true,
    ]
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if status == errSecSuccess, let result { return result as! SecKey }
    guard status == errSecItemNotFound else {
      throw APIError.credentials("Could not read this device's private key (\(status)).")
    }
    var error: Unmanaged<CFError>?
    let attrs: [String: Any] = [
      kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
      kSecAttrKeySizeInBits as String: 256,
      kSecPrivateKeyAttrs as String: [
        kSecAttrIsPermanent as String: true, kSecAttrApplicationTag as String: tag,
        kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
      ],
    ]
    guard let key = SecKeyCreateRandomKey(attrs as CFDictionary, &error) else {
      throw APIError.credentials("Could not create a protected device key.")
    }
    return key
  }
  private static func der(_ tag: UInt8, _ bytes: Data) -> Data {
    var length = Data()
    if bytes.count < 128 {
      length.append(UInt8(bytes.count))
    } else {
      var value = bytes.count
      var encoded = [UInt8]()
      while value > 0 {
        encoded.insert(UInt8(value & 255), at: 0)
        value >>= 8
      }
      length.append(0x80 | UInt8(encoded.count))
      length.append(contentsOf: encoded)
    }
    return Data([tag]) + length + bytes
  }
  static func csr(privateKey: SecKey) throws -> String {
    guard let publicKey = SecKeyCopyPublicKey(privateKey),
      let point = SecKeyCopyExternalRepresentation(publicKey, nil) as Data?
    else { throw APIError.credentials("Cannot read the device public key.") }
    let algorithm = Data([
      0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86,
      0x48, 0xce, 0x3d, 0x03, 0x01, 0x07,
    ])
    let publicInfo = der(0x30, algorithm + der(0x03, Data([0]) + point))
    let subject = der(
      0x30,
      der(
        0x31,
        der(
          0x30, Data([0x06, 0x03, 0x55, 0x04, 0x03]) + der(0x0c, Data("Control Plane device".utf8)))
      ))
    let request = der(0x30, Data([0x02, 0x01, 0x00]) + subject + publicInfo + Data([0xa0, 0x00]))
    guard
      let signature = SecKeyCreateSignature(
        privateKey, .ecdsaSignatureMessageX962SHA256, request as CFData, nil) as Data?
    else { throw APIError.credentials("Could not sign the device enrollment request.") }
    let signed = der(
      0x30,
      request + Data([0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02])
        + der(0x03, Data([0]) + signature))
    return "-----BEGIN CERTIFICATE REQUEST-----\n"
      + signed.base64EncodedString(options: .lineLength64Characters)
      + "\n-----END CERTIFICATE REQUEST-----\n"
  }
  private static func decodeCertificate(_ pem: String) throws -> SecCertificate {
    let encoded = pem.components(separatedBy: .newlines).filter { !$0.hasPrefix("-----") }.joined()
    guard pem.utf8.count < 65536, let bytes = Data(base64Encoded: encoded),
      let certificate = SecCertificateCreateWithData(nil, bytes as CFData)
    else { throw APIError.credentials("Invalid host certificate.") }
    return certificate
  }
  public static func certificate(host: String) -> DeviceCertificateInfo? {
    guard let text = CredentialStore.load(host: "device-identity:" + host),
      let data = text.data(using: .utf8)
    else { return nil }
    return try? JSONDecoder().decode(DeviceCertificateInfo.self, from: data)
  }
  public static func identity(host: String) throws -> SecIdentity? {
    guard let info = certificate(host: host) else { return nil }
    let query: [String: Any] = [
      kSecClass as String: kSecClassIdentity,
      kSecMatchLimit as String: kSecMatchLimitAll, kSecReturnRef as String: true,
    ]
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if status == errSecItemNotFound { return nil }
    guard status == errSecSuccess, let values = result as? [SecIdentity] else {
      throw APIError.credentials("Device identity is unavailable in Keychain (\(status)).")
    }
    for identity in values {
      var cert: SecCertificate?
      guard SecIdentityCopyCertificate(identity, &cert) == errSecSuccess, let cert else { continue }
      let fingerprint = SHA256.hash(data: SecCertificateCopyData(cert) as Data).map {
        String(format: "%02x", $0)
      }.joined()
      if fingerprint == info.fingerprint {
        var privateKey: SecKey?
        guard SecIdentityCopyPrivateKey(identity, &privateKey) == errSecSuccess, let privateKey,
          let publicKey = SecKeyCopyPublicKey(privateKey),
          let certificateKey = SecCertificateCopyKey(cert),
          let own = SecKeyCopyExternalRepresentation(publicKey, nil) as Data?,
          let issued = SecKeyCopyExternalRepresentation(certificateKey, nil) as Data?, own == issued
        else { throw APIError.credentials("Device identity has no matching private key.") }
        return identity
      }
    }
    throw APIError.credentials("Device certificate has no matching protected private key.")
  }
  public static func enroll(using client: APIClient) async throws -> DeviceCertificateInfo {
    guard client.baseURL.scheme == "https" else {
      throw APIError.credentials("Device certificates require an HTTPS connection.")
    }
    let host = client.baseURL.absoluteString
    let privateKey = try key(host)
    let response: DeviceCertificateInfo = try await client.request(
      "v1/certificates/enroll", method: "POST",
      body: ["csr_pem": .string(try csr(privateKey: privateKey))])
    return try accept(response, host: host, privateKey: privateKey)
  }
  public static func pair(using client: APIClient, secret: String, name: String, invitation: Bool)
    async throws -> PairResult
  {
    guard client.baseURL.scheme == "https" else {
      return try await client.request(
        invitation ? "v1/invitations/accept" : "v1/pair", method: "POST",
        body: ["secret": .string(secret), (invitation ? "name" : "device_name"): .string(name)])
    }
    let host = client.baseURL.absoluteString
    let privateKey = try key(client.baseURL.absoluteString)
    let result: PairResult = try await client.request(
      invitation ? "v1/invitations/accept" : "v1/pair", method: "POST",
      body: [
        "secret": .string(secret), (invitation ? "name" : "device_name"): .string(name),
        "csr_pem": .string(try csr(privateKey: privateKey)),
      ])
    guard let certificate = result.certificate, certificate.device_id == result.device_id else {
      throw APIError.credentials("Host pairing did not return this device’s certificate.")
    }
    _ = try accept(certificate, host: host, privateKey: privateKey)
    return result
  }
  private static func accept(_ response: DeviceCertificateInfo, host: String, privateKey: SecKey)
    throws -> DeviceCertificateInfo
  {
    let certificate = try decodeCertificate(response.certificate_pem)
    let ca = try decodeCertificate(response.ca_pem)
    let fingerprint = SHA256.hash(data: SecCertificateCopyData(certificate) as Data).map {
      String(format: "%02x", $0)
    }.joined()
    guard UUID(uuidString: response.device_id) != nil, fingerprint == response.fingerprint,
      response.expires_at > Int64(Date().timeIntervalSince1970),
      let certKey = SecCertificateCopyKey(certificate),
      let ownKey = SecKeyCopyPublicKey(privateKey),
      let certPoint = SecKeyCopyExternalRepresentation(certKey, nil) as Data?,
      let ownPoint = SecKeyCopyExternalRepresentation(ownKey, nil) as Data?, certPoint == ownPoint
    else { throw APIError.credentials("The issued certificate did not match this device's key.") }
    var trust: SecTrust?
    guard
      SecTrustCreateWithCertificates(
        [certificate, ca] as CFArray, SecPolicyCreateBasicX509(), &trust) == errSecSuccess,
      let trust
    else { throw APIError.credentials("Could not validate the certificate chain.") }
    SecTrustSetAnchorCertificates(trust, [ca] as CFArray)
    SecTrustSetAnchorCertificatesOnly(trust, true)
    guard SecTrustEvaluateWithError(trust, nil) else {
      throw APIError.credentials("The host returned an invalid device certificate chain.")
    }
    let attributes: [String: Any] = [
      kSecClass as String: kSecClassCertificate, kSecValueRef as String: certificate,
      kSecAttrLabel as String: label(host),
    ]
    let status = SecItemAdd(attributes as CFDictionary, nil)
    guard status == errSecSuccess || status == errSecDuplicateItem else {
      throw APIError.credentials("Could not save the device certificate (\(status)).")
    }
    try CredentialStore.save(
      String(decoding: JSONEncoder().encode(response), as: UTF8.self),
      host: "device-identity:" + host)
    guard try identity(host: host) != nil else {
      throw APIError.credentials(
        "Keychain did not associate the issued certificate with this device key.")
    }
    return response
  }
  public static func trustAnchor(host: String) -> SecCertificate? {
    guard let pem = CredentialStore.load(host: "host-ca:" + host) else { return nil }
    return try? decodeCertificate(pem)
  }
  public static func trustHostCA(_ pem: String, host: String, expectedSHA256: String) throws {
    let certificate = try decodeCertificate(pem)
    let fingerprint = SHA256.hash(data: SecCertificateCopyData(certificate) as Data).map {
      String(format: "%02x", $0)
    }.joined()
    guard expectedSHA256.lowercased().replacingOccurrences(of: ":", with: "") == fingerprint else {
      throw APIError.credentials("The host CA fingerprint does not match.")
    }
    try CredentialStore.save(pem, host: "host-ca:" + host)
  }
  public static func session(host: String) -> URLSession {
    let config = URLSessionConfiguration.ephemeral
    config.urlCache = nil
    config.httpCookieStorage = nil
    return URLSession(
      configuration: config, delegate: HostSessionDelegate(host: host), delegateQueue: nil)
  }
}
private final class HostSessionDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  let host: String
  init(host: String) { self.host = host }
  func urlSession(
    _ session: URLSession, task: URLSessionTask,
    willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
    completionHandler: @escaping (URLRequest?) -> Void
  ) { completionHandler(nil) }
  func urlSession(
    _ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
  ) {
    guard challenge.protectionSpace.host.lowercased() == URL(string: host)?.host?.lowercased()
    else {
      completionHandler(.cancelAuthenticationChallenge, nil)
      return
    }
    if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodClientCertificate
    {
      do {
        if let identity = try DeviceIdentity.identity(host: host) {
          completionHandler(
            .useCredential,
            URLCredential(identity: identity, certificates: nil, persistence: .forSession))
          return
        }
      } catch {
        completionHandler(.cancelAuthenticationChallenge, nil)
        return
      }
    }
    if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
      let ca = DeviceIdentity.trustAnchor(host: host),
      let trust = challenge.protectionSpace.serverTrust
    {
      SecTrustSetPolicies(
        trust, SecPolicyCreateSSL(true, challenge.protectionSpace.host as CFString))
      SecTrustSetAnchorCertificates(trust, [ca] as CFArray)
      SecTrustSetAnchorCertificatesOnly(trust, true)
      if SecTrustEvaluateWithError(trust, nil) {
        completionHandler(.useCredential, URLCredential(trust: trust))
      } else {
        completionHandler(.cancelAuthenticationChallenge, nil)
      }
      return
    }
    completionHandler(.performDefaultHandling, nil)
  }
}
