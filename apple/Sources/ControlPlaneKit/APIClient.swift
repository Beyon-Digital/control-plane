import Foundation

public final class APIClient: @unchecked Sendable {
    public let baseURL: URL
    private let token: String
    var credentialToken:String { token }
    let session: URLSession
    public let grpc:NativeGRPCClient?
    public init(address: String, token: String, session: URLSession? = nil,grpcAddress:String?=nil) throws {
        guard let url = URL(string: address), let host = url.host, url.user == nil, url.password == nil,
              url.query == nil, url.fragment == nil, ["https", "http"].contains(url.scheme ?? "") else { throw APIError.invalidAddress }
        guard url.scheme == "https" || ["localhost", "127.0.0.1", "::1", "[::1]"].contains(host) else { throw APIError.insecureAddress }
        self.baseURL = url; self.token = token; self.session = session ?? DeviceIdentity.session(host:url.absoluteString)
        let rpc=grpcAddress ?? UserDefaults.standard.string(forKey:"grpc:"+url.absoluteString)
        self.grpc=try rpc.flatMap{$0.isEmpty ? nil:try NativeGRPCClient(address:$0,httpBaseURL:url,token:token)}
    }
    public func request<T: Decodable>(_ path: String, method: String = "GET", query: [String: String] = [:], body: [String: JSONValue]? = nil) async throws -> T {
        if let grpc{return try JSONDecoder().decode(T.self,from:try await grpc.request(path:path,method:method,query:query,body:body))}
        var components = URLComponents(url: baseURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)!
        if !query.isEmpty { components.queryItems = query.sorted { $0.key < $1.key }.map { URLQueryItem(name: $0.key, value: $0.value) } }
        var request = URLRequest(url: components.url!); request.httpMethod = method
        request.setValue(token, forHTTPHeaderField: "X-Device-Token")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.timeoutInterval = 30
        if let body { request.httpBody = try JSONEncoder().encode(body); request.setValue("application/json", forHTTPHeaderField: "Content-Type") }
        let (data, response) = try await session.data(for: request)
        guard let response = response as? HTTPURLResponse else { throw APIError.invalidResponse }
        guard (200..<300).contains(response.statusCode) else {
            let payload = try? JSONDecoder().decode(JSONValue.self, from: data)
            throw APIError.server(response.statusCode, payload?["error"]?.text ?? HTTPURLResponse.localizedString(forStatusCode: response.statusCode))
        }
        return try JSONDecoder().decode(T.self, from: data.isEmpty ? Data("null".utf8) : data)
    }
    public func mutate(_ path: String, body: [String: JSONValue] = [:]) async throws {
        let _: JSONValue = try await request(path, method: "POST", body: body)
    }
    public func eventSocket(sessionID: String, after: Int64) -> URLSessionWebSocketTask {
        socket("v1/events",query:["session_id":sessionID,"after":String(after)])
    }
    public func socket(_ path:String,query:[String:String]=[:])->URLSessionWebSocketTask {
        var components = URLComponents(url: baseURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)!
        components.scheme = baseURL.scheme == "https" ? "wss" : "ws"
        components.queryItems = query.map{URLQueryItem(name:$0.key,value:$0.value)}
        var request = URLRequest(url: components.url!); request.setValue(token, forHTTPHeaderField: "X-Device-Token")
        return session.webSocketTask(with: request)
    }
}
