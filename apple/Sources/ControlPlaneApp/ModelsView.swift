import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif

@MainActor final class ModelSettingsState: ObservableObject {
  @Published var accounts: [ChatGPTAccount] = []
  @Published var busy = false
  @Published var error: String?
  let browser = ChatGPTBrowserSignIn()
  func reload() async {
    do { accounts = try await ChatGPTAuth.shared.accounts() } catch {
      self.error = error.localizedDescription
    }
  }
  func connect(accountID: String? = nil, onConnected: @escaping (ChatGPTAccount) -> Void) {
    guard !busy else { return }
    busy = true
    error = nil
    Task {
      defer { busy = false }
      do {
        let account = try await browser.signIn(accountID: accountID)
        await reload()
        onConnected(account)
      } catch is CancellationError {} catch {
        self.error = error.localizedDescription
        await reload()
      }
    }
  }
  func signOut(_ account: ChatGPTAccount) {
    guard !busy else { return }
    busy = true
    Task {
      do { try await ChatGPTAuth.shared.signOut(accountID: account.id) } catch {
        self.error = error.localizedDescription
      }
      await reload()
      busy = false
    }
  }
  func cancel() { browser.cancel() }
}

@MainActor struct ModelsView: View {
  @ObservedObject private var profiles = ModelProfileStore.shared
  @StateObject private var settings = ModelSettingsState()
  @AppStorage("controlPlane.chatGPTPlanWelcomeSeen") private var welcomeSeen = false
  @State private var editing: ModelProfile?
  @State private var deleting: ModelProfile?
  @State private var loggingOut: ChatGPTAccount?
  @State private var pendingAccount: ChatGPTAccount?
  @State private var showWelcome = false
  var onCredentialsChange: () -> Void = {}
  var body: some View {
    Form {
      Section {
        Button("Continue with ChatGPT", systemImage: "person.crop.circle") {
          onCredentialsChange()
          settings.connect { connected($0) }
        }.disabled(settings.busy)
        Button("Add custom connection", systemImage: "plus") { editing = ModelProfile() }
          .accessibilityIdentifier("models.add")
        if settings.busy {
          HStack { ProgressView("Connecting account…"); Spacer(); Button("Cancel") { settings.cancel() } }
        }
      } header: { Text("Add a connection") } footer: {
        Text("These connections power Chat. Coding agents use the accounts on their machine. Keys stay in this device's Keychain.")
      }
      if !profiles.profiles.isEmpty {
        Section {
          ForEach(profiles.profiles) { profile in
            HStack(spacing: 12) {
              Button {
                if profiles.selectedID != profile.id { onCredentialsChange(); profiles.selectedID = profile.id }
              } label: {
                HStack {
                  VStack(alignment: .leading, spacing: 5) {
                    Text(profile.name).fontWeight(.medium).foregroundStyle(.primary)
                    Text(profile.model).font(.subheadline).foregroundStyle(.secondary)
                    Text(profile.auth == .chatGPT ? "ChatGPT" : profile.baseURL).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                  }
                  Spacer(minLength: 8)
                  if profiles.selectedID == profile.id { Image(systemName: "checkmark").fontWeight(.semibold).accessibilityLabel("Selected model") }
                }.frame(minHeight: 44).contentShape(Rectangle())
              }.buttonStyle(.plain)
              Menu {
                Button("Edit", systemImage: "pencil") { editing = profile }
                Button("Delete", systemImage: "trash", role: .destructive) { deleting = profile }
              } label: { Image(systemName: "ellipsis").frame(width: 44, height: 44) }
                .accessibilityLabel("Actions for \(profile.name)")
            }
          }
        } header: { Text("Chat model") } footer: { Text("Tap a connection to use it in Chat.") }
      }
      if !settings.accounts.isEmpty {
        Section {
          ForEach(settings.accounts, id: \.id) { account in
            VStack(alignment: .leading, spacing: 8) {
              Text(account.label).fontWeight(.medium)
              Label(account.canInfer ? "Inference scope available" : "Reconnect to check access", systemImage: account.canInfer ? "checkmark.seal" : "exclamationmark.circle")
                .font(.caption).foregroundStyle(account.canInfer ? Color.green : Color.orange)
              HStack {
                Button("Choose model") { editing = chatGPTProfile(account) }.disabled(!account.canInfer || settings.busy)
                Spacer()
                Menu("Account options") {
                  Button("Reconnect") { onCredentialsChange(); settings.connect(accountID: account.id) { connected($0) } }
                  Button("Sign out", role: .destructive) { loggingOut = account }
                }.disabled(settings.busy)
              }.buttonStyle(.bordered)
            }.padding(.vertical, 5)
          }
          Link("Manage ChatGPT usage", destination: URL(string: "https://chatgpt.com/settings/usage")!)
        } header: { Text("ChatGPT accounts") } footer: { Text("Model access depends on your account. Connecting does not confirm access.") }
      }
      if let error = settings.error { Section { Text(error).foregroundStyle(.red).textSelection(.enabled) } }
    }
    .formStyle(.grouped).navigationTitle("Models")
    #if os(iOS)
    .navigationBarTitleDisplayMode(.inline)
    #endif
    .task { await settings.reload() }
    .onDisappear { if !settings.browser.isPresentingBrowser { settings.cancel() } }
    .sheet(item: $editing) { profile in
      NavigationStack { ModelProfileEditor(profile: profile, accounts: settings.accounts, onCredentialsChange: onCredentialsChange) }
    }
    .sheet(isPresented: $showWelcome, onDismiss: {
      welcomeSeen = true
      if let account = pendingAccount { editing = chatGPTProfile(account); pendingAccount = nil }
    }) {
      VStack(alignment: .leading, spacing: 20) {
        Text("ChatGPT connected").font(.title2.bold())
        Text("Choose a model to use this account. Requests need eligible plan access.")
        Link("Manage usage", destination: URL(string: "https://chatgpt.com/settings/usage")!)
        Button("Continue") { showWelcome = false }.buttonStyle(.borderedProminent)
      }.padding(28)
    }
    .confirmationDialog("Delete this model connection?", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }), titleVisibility: .visible) {
      Button("Delete connection", role: .destructive) { onCredentialsChange(); if let profile = deleting { profiles.delete(id: profile.id) }; deleting = nil }
    }
    .confirmationDialog("Sign out of this ChatGPT account on this device?", isPresented: Binding(get: { loggingOut != nil }, set: { if !$0 { loggingOut = nil } }), titleVisibility: .visible) {
      Button("Sign out", role: .destructive) { onCredentialsChange(); if let account = loggingOut { settings.signOut(account) }; loggingOut = nil }
    }
  }
  private func connected(_ account: ChatGPTAccount) {
    guard account.canInfer else {
      settings.error =
        "Signed in, but this account has no inference scope. Reconnect with eligible authorization or add an API-key model connection."
      return
    }
    if !welcomeSeen {
      pendingAccount = account
      showWelcome = true
    } else {
      editing = chatGPTProfile(account)
    }
  }
  private func chatGPTProfile(_ account: ChatGPTAccount) -> ModelProfile {
    ModelProfile(
      name: "ChatGPT · \(account.label)", baseURL: "https://api.openai.com/v1", api: .responses,
      auth: .chatGPT, chatGPTAccountID: account.id)
  }
}

@MainActor struct ModelProfileEditor: View {
  @Environment(\.dismiss) private var dismiss
  @ObservedObject private var store = ModelProfileStore.shared
  @State private var profile: ModelProfile
  @State private var key = ""
  @State private var catalog: [String] = []
  @State private var loading = false
  @State private var error: String?
  @State private var discovery: Task<Void, Never>?
  @State private var showAdvanced = false
  @State private var showCatalog = false
  @State private var catalogQuery = ""
  private let accounts: [ChatGPTAccount]
  private let onCredentialsChange: () -> Void
  init(
    profile: ModelProfile, accounts: [ChatGPTAccount],
    onCredentialsChange: @escaping () -> Void = {}
  ) {
    _profile = State(initialValue: profile)
    self.accounts = accounts
    self.onCredentialsChange = onCredentialsChange
  }
  var body: some View {
    Form {
      Section("Connection") {
        TextField("Connection name", text: $profile.name).accessibilityIdentifier("model.name")
        if profile.auth == .apiKey {
          TextField("Endpoint, including /v1", text: $profile.baseURL).autocorrectionDisabled().accessibilityIdentifier("model.url")
            #if os(iOS)
            .keyboardType(.URL).textInputAutocapitalization(.never)
            #endif
          SecureField(store.apiKey(for: profile) == nil ? "API key (optional)" : "Saved key retained", text: $key)
          Text("Changing the endpoint clears its saved key unless you enter a replacement.").font(.caption).foregroundStyle(.secondary)
        } else {
          Picker("ChatGPT account", selection: Binding(get: { profile.chatGPTAccountID ?? "" }, set: { profile.chatGPTAccountID = $0 })) {
            Text("Choose account").tag("")
            ForEach(accounts, id: \.id) { account in Text(account.label + (account.canInfer ? "" : " · access unavailable")).tag(account.id) }
          }
          Text("Uses OpenAI Responses with this account's authorized inference scope.").font(.caption).foregroundStyle(.secondary)
        }
      }
      Section {
        TextField("Model ID", text: $profile.model).autocorrectionDisabled().accessibilityIdentifier("model.id")
          #if os(iOS)
          .textInputAutocapitalization(.never)
          #endif
        Button(loading ? "Loading models…" : "Browse models", systemImage: "list.bullet") { loadModels() }.disabled(loading)
      } header: { Text("Model") } footer: { Text("Enter a model ID directly if this endpoint does not support discovery.") }
      if profile.auth == .apiKey {
        Section {
          DisclosureGroup("API options · \(profile.api == .responses ? "Responses" : "Chat Completions")", isExpanded: $showAdvanced) {
            Picker("API", selection: $profile.api) {
              Text("Responses").tag(ModelAPI.responses)
              Text("Chat Completions").tag(ModelAPI.chatCompletions)
            }
            if profile.api == .chatCompletions {
              Picker("Output limit field", selection: Binding(get: { profile.chatTokenParameter ?? .maxCompletionTokens }, set: { profile.chatTokenParameter = $0 })) {
                Text("max_completion_tokens").tag(ChatTokenParameter.maxCompletionTokens)
                Text("max_tokens").tag(ChatTokenParameter.maxTokens)
              }
            }
            Text("Up to 8,192 output tokens per request.").font(.caption).foregroundStyle(.secondary)
          }
        } footer: { Text("Prompts and tool results are sent to this endpoint. HTTPS is required except on localhost.") }
      }
      if let error { Section { Text(error).foregroundStyle(.red).textSelection(.enabled) } }
    }
    .formStyle(.grouped)
    .navigationTitle(store.profiles.contains(where: { $0.id == profile.id }) ? "Edit connection" : "Add connection")
    #if os(iOS)
    .navigationBarTitleDisplayMode(.inline)
    .scrollDismissesKeyboard(.interactively)
    #endif
    .toolbar {
      ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
      ToolbarItem(placement: .confirmationAction) {
        Button("Save") { save() }.accessibilityIdentifier("model.save").disabled(loading || profile.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || profile.model.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
      }
    }
    #if os(macOS)
    .frame(minWidth: 400, idealWidth: 500, minHeight: 440)
    #endif
    .sheet(isPresented: $showCatalog) {
      NavigationStack {
        List(catalog.filter { catalogQuery.isEmpty || $0.localizedCaseInsensitiveContains(catalogQuery) }, id: \.self) { model in
          Button { profile.model = model; showCatalog = false } label: {
            HStack { Text(model).foregroundStyle(.primary); Spacer(); if profile.model == model { Image(systemName: "checkmark") } }
          }
        }.searchable(text: $catalogQuery, prompt: "Find a model").navigationTitle("Choose a model")
          .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { showCatalog = false } } }
      }
    }
    .onChange(of: profile.baseURL) { _, _ in resetDiscovery() }
    .onChange(of: profile.api) { _, _ in resetDiscovery() }
    .onChange(of: profile.chatGPTAccountID) { _, _ in resetDiscovery() }
    .onChange(of: key) { _, _ in resetDiscovery() }
    .onDisappear { discovery?.cancel() }
  }
  private func resetDiscovery() {
    discovery?.cancel()
    discovery = nil
    catalog = []
    loading = false
  }
  private func loadModels() {
    resetDiscovery()
    loading = true
    error = nil
    let draft = profile
    let apiKey = key.isEmpty ? nil : key
    discovery = Task {
      defer { if !Task.isCancelled { loading = false } }
      do {
        let result = try await ModelInference().models(profile: draft, apiKeyOverride: apiKey)
        try Task.checkCancellation()
        catalog = result
        catalogQuery = ""
        if result.isEmpty { error = "No models returned. Enter a model ID instead." } else { showCatalog = true }
      } catch is CancellationError {} catch {
        if !Task.isCancelled { self.error = error.localizedDescription }
      }
    }
  }
  private func save() {
    do {
      profile.name = profile.name.trimmingCharacters(in: .whitespacesAndNewlines)
      profile.model = profile.model.trimmingCharacters(in: .whitespacesAndNewlines)
      if profile.auth == .chatGPT {
        guard accounts.contains(where: { $0.id == profile.chatGPTAccountID && $0.canInfer }) else {
          throw APIError.server(403, "Choose a connected account with inference scope.")
        }
        profile.baseURL = "https://api.openai.com/v1"
        profile.api = .responses
      }
      _ = try profile.validatedURL()
      onCredentialsChange()
      try store.save(profile, apiKey: key.isEmpty ? nil : key)
      if store.selectedID == nil { store.selectedID = profile.id }
      key = ""
      dismiss()
    } catch { self.error = error.localizedDescription }
  }
}
