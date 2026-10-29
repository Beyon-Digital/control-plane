import Combine
import Foundation

public enum ModelAPI: String, Codable, CaseIterable, Hashable, Sendable {
  case chatCompletions, responses
}
public enum ModelAuthentication: String, Codable, CaseIterable, Hashable, Sendable {
  case apiKey, chatGPT
}
public enum ChatTokenParameter: String, Codable, Hashable, Sendable {
  case maxCompletionTokens = "max_completion_tokens"
  case maxTokens = "max_tokens"
}
public struct ModelProfile: Codable, Identifiable, Hashable, Sendable {
  public var id: String
  public var name: String
  public var baseURL: String
  public var model: String
  public var api: ModelAPI
  public var auth: ModelAuthentication
  public var chatGPTAccountID: String?
  public var chatTokenParameter: ChatTokenParameter?
  public init(
    id: String = UUID().uuidString, name: String = "New model",
    baseURL: String = "https://api.openai.com/v1", model: String = "", api: ModelAPI = .responses,
    auth: ModelAuthentication = .apiKey, chatGPTAccountID: String? = nil,
    chatTokenParameter: ChatTokenParameter? = nil
  ) {
    self.id = id
    self.name = name
    self.baseURL = baseURL
    self.model = model
    self.api = api
    self.auth = auth
    self.chatGPTAccountID = chatGPTAccountID
    self.chatTokenParameter = chatTokenParameter
  }
  public func validatedURL(requireModel: Bool = true) throws -> URL {
    guard UUID(uuidString: id) != nil,
      !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, name.utf8.count <= 256,
      !requireModel
        || (!model.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
          && model.utf8.count <= 256),
      var parts = URLComponents(string: baseURL.trimmingCharacters(in: .whitespacesAndNewlines)),
      let host = parts.host, !host.isEmpty,
      parts.user == nil, parts.password == nil, parts.query == nil, parts.fragment == nil,
      let scheme = parts.scheme?.lowercased(), ["http", "https"].contains(scheme),
      parts.port == nil || (1...65535).contains(parts.port!)
    else { throw ModelInferenceError.invalidProfile }
    guard scheme == "https" || Self.isLoopback(host) else {
      throw ModelInferenceError.insecureEndpoint
    }
    parts.scheme = scheme
    while parts.path.hasSuffix("/") { parts.path.removeLast() }
    guard let url = parts.url else { throw ModelInferenceError.invalidProfile }
    if auth == .chatGPT {
      guard url.absoluteString == "https://api.openai.com/v1", api == .responses,
        let account = chatGPTAccountID, !account.isEmpty
      else { throw ModelInferenceError.invalidChatGPTProfile }
    }
    return url
  }
  static func isLoopback(_ host: String) -> Bool {
    ["localhost", "127.0.0.1", "::1", "[::1]"].contains(host.lowercased())
  }
  var credentialKey: String { "orchestrator-model:" + id }
  static func storedAPIKey(
    for profile: ModelProfile, load: (String) -> String? = { CredentialStore.load(host: $0) }
  ) -> String? {
    guard profile.auth == .apiKey, let endpoint = try? profile.validatedURL(requireModel: false),
      let data = load(profile.credentialKey)?.data(using: .utf8),
      let record = try? JSONDecoder().decode(ModelCredential.self, from: data),
      record.baseURL == endpoint.absoluteString
    else { return nil }
    return record.token
  }
}
private struct ModelCredential: Codable {
  let baseURL: String
  let token: String
}
@MainActor public final class ModelProfileStore: ObservableObject {
  public static let shared = ModelProfileStore()
  @Published public private(set) var profiles: [ModelProfile]
  @Published public private(set) var revision: Int = 0
  @Published public var selectedID: String? {
    didSet {
      defaults.set(selectedID, forKey: selectedKey)
      revision &+= 1
    }
  }
  private let defaults: UserDefaults
  private let profilesKey = "controlplane.orchestrator.models.v1"
  private let selectedKey = "controlplane.orchestrator.selected-model.v1"
  public var selected: ModelProfile? { profiles.first { $0.id == selectedID } }
  public init(defaults: UserDefaults = .standard) {
    self.defaults = defaults
    profiles =
      defaults.data(forKey: profilesKey).flatMap {
        try? JSONDecoder().decode([ModelProfile].self, from: $0)
      } ?? []
    selectedID = defaults.string(forKey: selectedKey)
    if !profiles.contains(where: { $0.id == selectedID }) { selectedID = profiles.first?.id }
  }
  public func save(_ profile: ModelProfile, apiKey: String? = nil) throws {
    let endpoint = try profile.validatedURL(requireModel: false)
    if profile.auth == .chatGPT {
      CredentialStore.delete(host: profile.credentialKey)
    } else if let apiKey {
      let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
      if key.isEmpty {
        CredentialStore.delete(host: profile.credentialKey)
      } else {
        guard key.utf8.count <= 16384, !key.contains(where: { $0.isNewline || $0 == "\0" }) else {
          throw ModelInferenceError.invalidCredential
        }
        let data = try JSONEncoder().encode(
          ModelCredential(baseURL: endpoint.absoluteString, token: key))
        try CredentialStore.save(String(decoding: data, as: UTF8.self), host: profile.credentialKey)
      }
    } else if let old = profiles.first(where: { $0.id == profile.id }),
      (try? old.validatedURL(requireModel: false)) != endpoint
    {
      CredentialStore.delete(host: profile.credentialKey)
    }
    if let index = profiles.firstIndex(where: { $0.id == profile.id }) {
      profiles[index] = profile
    } else {
      profiles.append(profile)
    }
    defaults.set(try JSONEncoder().encode(profiles), forKey: profilesKey)
    if selectedID == nil { selectedID = profile.id }
    revision &+= 1
  }
  public func delete(id: String) {
    CredentialStore.delete(host: "orchestrator-model:" + id)
    profiles.removeAll { $0.id == id }
    defaults.set(try? JSONEncoder().encode(profiles), forKey: profilesKey)
    if selectedID == id { selectedID = profiles.first?.id }
    revision &+= 1
  }
  public func apiKey(for profile: ModelProfile) -> String? {
    ModelProfile.storedAPIKey(for: profile)
  }
}
