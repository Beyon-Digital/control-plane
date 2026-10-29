import CryptoKit
import Foundation

extension APIClient {
  private func binaryRequest(sessionID: String, path: String, method: String) -> URLRequest {
    var url = URLComponents(
      url: baseURL.appendingPathComponent("v1/workspace/transfer"), resolvingAgainstBaseURL: false)!
    url.queryItems = [
      URLQueryItem(name: "session_id", value: sessionID), URLQueryItem(name: "path", value: path),
    ]
    var request = URLRequest(url: url.url!)
    request.httpMethod = method
    request.timeoutInterval = 300
    request.setValue(credentialToken, forHTTPHeaderField: "X-Device-Token")
    return request
  }
  private func binaryResponse(_ response: URLResponse) throws -> HTTPURLResponse {
    guard let response = response as? HTTPURLResponse else { throw APIError.invalidResponse }
    guard (200..<300).contains(response.statusCode) else {
      throw APIError.server(
        response.statusCode, HTTPURLResponse.localizedString(forStatusCode: response.statusCode))
    }
    return response
  }
  public func uploadFile(from source: URL, sessionID: String, path: String, replaceExisting: Bool)
    async throws -> JSONValue
  {
    let maximum: UInt64 = 1024 * 1024 * 1024
    let snapshot = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    guard
      FileManager.default.createFile(
        atPath: snapshot.path, contents: nil, attributes: [.posixPermissions: 0o600])
    else { throw APIError.credentials("Cannot create a private upload snapshot.") }
    defer { try? FileManager.default.removeItem(at: snapshot) }
    let reader = try FileHandle(forReadingFrom: source)
    let writer = try FileHandle(forWritingTo: snapshot)
    defer {
      try? reader.close()
      try? writer.close()
    }
    var size: UInt64 = 0
    var hash = SHA256()
    while let chunk = try reader.read(upToCount: 65536), !chunk.isEmpty {
      try Task.checkCancellation()
      size += UInt64(chunk.count)
      guard size <= maximum else { throw APIError.credentials("The upload exceeds 1 GiB.") }
      hash.update(data: chunk)
      try writer.write(contentsOf: chunk)
    }
    try writer.synchronize()
    try writer.close()
    var request = binaryRequest(sessionID: sessionID, path: path, method: "POST")
    if replaceExisting {
      let (_, response) = try await session.data(
        for: binaryRequest(sessionID: sessionID, path: path, method: "HEAD"))
      let metadata = try binaryResponse(response)
      guard let etag = metadata.value(forHTTPHeaderField: "ETag"), etag.count == 66 else {
        throw APIError.invalidResponse
      }
      request.setValue(etag, forHTTPHeaderField: "If-Match")
    } else {
      request.setValue("*", forHTTPHeaderField: "If-None-Match")
    }
    request.setValue(String(size), forHTTPHeaderField: "Content-Length")
    request.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
    let (data, response) = try await session.upload(for: request, fromFile: snapshot)
    _ = try binaryResponse(response)
    let result = try JSONDecoder().decode(JSONValue.self, from: data)
    let expected = hash.finalize().map { String(format: "%02x", $0) }.joined()
    guard result["sha256"]?.text == expected else {
      throw APIError.credentials("Upload checksum verification failed.")
    }
    return result
  }
  public func downloadFile(sessionID: String, path: String) async throws -> URL {
    let (bytes, response) = try await session.bytes(
      for: binaryRequest(sessionID: sessionID, path: path, method: "GET"))
    let info = try binaryResponse(response)
    let maximum: Int64 = 1024 * 1024 * 1024
    guard info.expectedContentLength >= 0, info.expectedContentLength <= maximum,
      let expected = info.value(forHTTPHeaderField: "X-Content-SHA256"), expected.count == 64
    else { throw APIError.invalidResponse }
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    guard
      FileManager.default.createFile(
        atPath: url.path, contents: nil, attributes: [.posixPermissions: 0o600])
    else { throw APIError.credentials("Cannot create a private download file.") }
    var keep = false
    defer { if !keep { try? FileManager.default.removeItem(at: url) } }
    let file = try FileHandle(forWritingTo: url)
    defer { try? file.close() }
    var count: Int64 = 0
    var hash = SHA256()
    var chunk = Data()
    chunk.reserveCapacity(65536)
    for try await byte in bytes {
      try Task.checkCancellation()
      count += 1
      guard count <= maximum, count <= info.expectedContentLength else {
        throw APIError.credentials("Download exceeds its declared size.")
      }
      chunk.append(byte)
      if chunk.count == 65536 {
        hash.update(data: chunk)
        try file.write(contentsOf: chunk)
        chunk.removeAll(keepingCapacity: true)
      }
    }
    hash.update(data: chunk)
    try file.write(contentsOf: chunk)
    try file.synchronize()
    guard count == info.expectedContentLength,
      hash.finalize().map({ String(format: "%02x", $0) }).joined() == expected.lowercased()
    else { throw APIError.credentials("Download was truncated or its checksum changed.") }
    keep = true
    return url
  }
}
