import Foundation

public enum JSONValue: Codable, Equatable, Sendable {
    case string(String), number(Double), bool(Bool), object([String: JSONValue]), array([JSONValue]), null
    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(Double.self) { self = .number(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([String: JSONValue].self) { self = .object(v) }
        else { self = .array(try c.decode([JSONValue].self)) }
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
    public var text: String {
        switch self {
        case .string(let v): return v
        case .object(let v):
            for key in ["text", "message", "delta", "content", "error"] { if let value = v[key] { return value.text } }
            return prettyPrinted
        default: return prettyPrinted
        }
    }
    public var prettyPrinted: String {
        let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        return (try? String(data: encoder.encode(self), encoding: .utf8)) ?? ""
    }
    public subscript(_ key: String) -> JSONValue? { if case .object(let values) = self { return values[key] }; return nil }
}

public struct HostInfo: Decodable, Sendable {
    public let id: String
    public let name: String
    public let os: String
    public let protocol_version: Int?
    public let providers: [ProviderInfo]
    public let desktop: JSONValue?
    public let workspace_roots: [String]?
}
public struct ProviderInfo: Decodable, Identifiable, Sendable {
    public var id: String { name }
    public let name: String
    public let available: Bool
    public let version: String?
    public let detail: String?
}
public struct AgentSession: Decodable, Identifiable, Sendable {
    public let id: String
    public let provider: String
    public let workspace: String
    public let title: String
    public let state: String
    public let owner: String?
    public let created_at: JSONValue?
    public let provider_session_id: String?
    public let shared: Bool?
    public let execution_target: JSONValue?
    public let trusted: Bool?
}
public struct ExecutionDestination: Decodable, Identifiable, Sendable {
    public let execution_target: JSONValue
    public let name: String?
    public let user: String?
    public let roots: [String]?
    public var id: String { execution_target["kind"]?.text == "wsl" ? "wsl:" + (execution_target["distribution"]?.text ?? "") : "native" }
    public var displayName: String { id == "native" ? "Host OS" : "WSL · \(execution_target["distribution"]?.text ?? "")" }
}
public struct SessionEvent: Codable, Identifiable, Sendable {
    public var id: Int64 { sequence }
    public let sequence: Int64
    public let session_id: String
    public let kind: String
    public let payload: JSONValue
    public let created_at: JSONValue?
    public init(sequence:Int64,session_id:String,kind:String,payload:JSONValue,created_at:JSONValue?){self.sequence=sequence;self.session_id=session_id;self.kind=kind;self.payload=payload;self.created_at=created_at}
}
public struct DirectoryListing: Decodable, Sendable { public let entries: [FileEntry] }
public struct FileEntry: Decodable, Identifiable, Sendable {
    public var id: String { path }
    public let name: String
    public let path: String
    public let is_dir: Bool
    public let size: UInt64?
}
public struct FileContent: Decodable, Sendable {
    public let path: String
    public let content: String
    public let sha256: String
}
public struct CommandResult: Decodable, Sendable { public let stdout: String; public let stderr: String; public let success: Bool;public let worktree_path:String? }
public struct DesktopFrame: Decodable, Sendable {
    public let image_base64: String
    public let mime_type: String
    public let geometry_version: JSONValue
    public let width: Double
    public let height: Double
}
public struct PairResult: Decodable, Sendable { public let token: String; public let device_id: String; public let certificate:DeviceCertificateInfo? }

public enum APIError: LocalizedError {
    case invalidAddress, insecureAddress, server(Int, String), invalidResponse, credentials(String)
    public var errorDescription: String? {
        switch self {
        case .invalidAddress: return "Enter a valid host URL, including https:// and its port."
        case .insecureAddress: return "Use HTTPS for remote hosts. HTTP is only allowed on loopback for development."
        case .server(let code, let message): return "\(message) (\(code))"
        case .invalidResponse: return "The host returned an invalid response."
        case .credentials(let message): return message
        }
    }
}
