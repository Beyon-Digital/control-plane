import Foundation

public struct TerminalWireMessage: Decodable, Sendable {
  public let type: String
  public let sequence: UInt64?
  public let data: String?
  public let code: Int?
  public let message: String?
  public let error: String?
  public func outputBytes() throws -> Data {
    guard type == "output", let data, data.utf8.count <= 4 * ((65536 + 2) / 3),
      let decoded = Data(base64Encoded: data), decoded.count <= 65536
    else { throw APIError.invalidResponse }
    return decoded
  }
}
