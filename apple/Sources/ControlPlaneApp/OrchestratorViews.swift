import SwiftUI
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@MainActor struct OrchestratorView: View {
    @EnvironmentObject private var host: AppModel
    @ObservedObject var conversation: OrchestratorModel
    @ObservedObject private var profiles = ModelProfileStore.shared
    @State private var showModels = false
    @State private var showContext = false
    @State private var confirmClear = false
    @State private var accounts: [ChatGPTAccount] = []
    @FocusState private var composing: Bool
    private var account: ChatGPTAccount? { accounts.first { $0.id == profiles.selected?.chatGPTAccountID } }
    private var ready: Bool { profiles.selected.map { $0.auth != .chatGPT || account?.canInfer == true } ?? false }

    var body: some View {
        VStack(spacing: 0) {
            if !profiles.profiles.isEmpty { modelBar }
            if profiles.profiles.isEmpty { setup } else { transcript }
            if let error = conversation.error {
                VStack(alignment: .leading, spacing: 8) {
                    Text(error).foregroundStyle(.red).textSelection(.enabled)
                    if profiles.selected?.auth == .chatGPT {
                        Button("Review account") { showModels = true }
                    }
                }.font(.callout).frame(maxWidth: .infinity, alignment: .leading).padding()
            }
        }
        #if os(iOS)
        .navigationTitle("Chat")
        .navigationBarTitleDisplayMode(.inline)
        #else
        .navigationTitle("Orchestrator")
        #endif
        .safeAreaInset(edge: .bottom) { if !profiles.profiles.isEmpty { composer } }
        .toolbar {
            ToolbarItem {
                Button("Models", systemImage: "slider.horizontal.3") { showModels = true }
                    .accessibilityIdentifier("orchestrator.models")
            }
            if !conversation.entries.isEmpty {
                ToolbarItem {
                    Button("New conversation", systemImage: "square.and.pencil") { confirmClear = true }
                }
            }
        }
        .confirmationDialog("Start a new conversation?", isPresented: $confirmClear, titleVisibility: .visible) {
            Button("New conversation") { conversation.reset() }
        }
        .sheet(isPresented: $showModels, onDismiss: reloadAccounts) {
            NavigationStack {
                ModelsView(onCredentialsChange: conversation.credentialsWillChange)
                    .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { showModels = false } } }
            }
        }
        .sheet(isPresented: $showContext) { context }
        .onAppear { conversation.bind(host: host.currentAddress, profile: profiles.selected); reloadAccounts() }
        .onChange(of: host.currentAddress) { _, address in conversation.bind(host: address, profile: profiles.selected) }
        .onChange(of: profiles.selected) { _, profile in conversation.bind(host: host.currentAddress, profile: profile); reloadAccounts() }
        .onChange(of: profiles.revision) { _, _ in conversation.credentialsWillChange(); reloadAccounts() }
        .onChange(of: conversation.running) { before, after in if before && !after && host.client != nil { host.perform { try await host.refresh() } } }
    }
    private var modelBar: some View {
        HStack(spacing: 12) {
            Menu {
                Picker("Chat model", selection: $profiles.selectedID) {
                    Text("Choose a model").tag(Optional<String>.none)
                    ForEach(profiles.profiles) { profile in Text(profile.name).tag(Optional(profile.id)) }
                }.pickerStyle(.inline)
            } label: {
                HStack(spacing: 7) {
                    Text(profiles.selected?.name ?? "Choose a model").font(.subheadline.weight(.semibold)).lineLimit(1)
                    Image(systemName: "chevron.down").font(.caption.weight(.semibold))
                }.frame(minHeight: 44)
            }.accessibilityLabel("Chat model").layoutPriority(1)
            Spacer(minLength: 0)
            Button { showContext = true } label: {
                if host.client == nil { Image(systemName: "info.circle").frame(width: 44, height: 44).accessibilityLabel("Chat settings") }
                else { Label(conversation.allowManagement ? "Can manage" : "Read only", systemImage: "desktopcomputer").font(.caption.weight(.medium)).lineLimit(1).frame(minHeight: 44) }
            }.buttonStyle(.plain).foregroundStyle(.secondary)
        }.padding(.horizontal).background(.bar)
    }
    private var setup: some View {
        VStack(spacing: 18) {
            Spacer(minLength: 30)
            Image(systemName: "bubble.left.and.bubble.right").font(.system(size: 42)).foregroundStyle(.indigo)
            Text("Choose your model").font(.title2.weight(.semibold))
            Text("Connect ChatGPT or add your preferred model endpoint.")
                .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 300)
            Button("Set up a model") { showModels = true }.buttonStyle(.borderedProminent).controlSize(.large)
            Spacer(minLength: 30)
        }.padding(28).frame(maxWidth: .infinity, maxHeight: .infinity)
    }
    private var transcript: some View {
        ScrollViewReader { reader in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 24) {
                    if conversation.entries.isEmpty {
                        VStack(alignment: .leading, spacing: 16) {
                            Text("What are we working on?").font(.title2.weight(.semibold))
                            Text(host.client == nil ? "Plan a change or work through a coding question." : "Ask about your agents or plan the next change.").foregroundStyle(.secondary)
                            ForEach(host.client == nil ? ["Help me plan a coding task."] : ["Summarize this machine's active sessions.", "Help me plan a coding task."], id: \.self) { suggestion in
                                Button(suggestion) { conversation.draft = suggestion; composing = true }.buttonStyle(.bordered)
                            }
                        }.padding(.vertical, 28)
                    }
                    ForEach(conversation.entries) { entry in
                        HStack(alignment: .top) {
                            if entry.role == "user" { Spacer(minLength: 36) }
                            VStack(alignment: .leading, spacing: 8) {
                                if entry.text.isEmpty && conversation.running { ProgressView("Thinking…") }
                                else { AgentMessageText(text: entry.text).frame(maxWidth: .infinity, alignment: .leading) }
                                if let note = entry.note { Text(note).font(.caption).foregroundStyle(.orange) }
                            }
                            .padding(entry.role == "user" ? 14 : 0)
                            .background(entry.role == "user" ? Color.indigo.opacity(0.08) : .clear, in: RoundedRectangle(cornerRadius: 16))
                            if entry.role != "user" { Spacer(minLength: 0) }
                        }.id(entry.id)
                    }
                    if !conversation.tools.isEmpty {
                        DisclosureGroup("Host activity · \(conversation.tools.count)") {
                            ForEach(Array(conversation.tools.enumerated()), id: \.offset) { _, tool in Text(tool).font(.caption.monospaced()).textSelection(.enabled).padding(.vertical, 3) }
                        }.font(.caption).foregroundStyle(.secondary)
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }.padding(20).frame(maxWidth: 850).frame(maxWidth: .infinity)
            }
            .onChange(of: conversation.entries.last?.text) { _, _ in reader.scrollTo("bottom", anchor: .bottom) }
            .onChange(of: conversation.entries.count) { _, _ in reader.scrollTo("bottom", anchor: .bottom) }
        }
    }
    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .bottom, spacing: 10) {
                TextField("Message…", text: $conversation.draft, axis: .vertical)
                    .lineLimit(1...6).textFieldStyle(.plain).focused($composing)
                    .accessibilityLabel("Orchestrator message")
                    .padding(13).background(.quaternary, in: RoundedRectangle(cornerRadius: 18))
                if conversation.running {
                    Button { conversation.cancel() } label: { Image(systemName: "stop.fill").frame(width: 24, height: 24) }
                        .buttonStyle(.bordered).buttonBorderShape(.circle).controlSize(.large).accessibilityLabel("Stop response")
                } else {
                    Button {
                        if let profile = profiles.selected { conversation.send(profile: profile, client: host.client, host: host.currentAddress); composing = false }
                    } label: { Image(systemName: "arrow.up").fontWeight(.semibold).frame(width: 24, height: 24) }
                    .buttonStyle(.borderedProminent).buttonBorderShape(.circle).controlSize(.large).accessibilityLabel("Send message")
                    .disabled(!ready || conversation.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            if !conversation.status.isEmpty && conversation.status != "New conversation" {
                Text(conversation.status).font(.caption).foregroundStyle(.secondary).lineLimit(3)
            }
        }.padding(.horizontal, 16).padding(.vertical, 12).background(.bar)
    }
    private var context: some View {
        NavigationStack {
            Form {
                if let profile = profiles.selected {
                    Section("Model") {
                        LabeledContent(profile.name, value: profile.model)
                        Text(profile.baseURL).font(.caption).textSelection(.enabled)
                        if profile.auth == .chatGPT {
                            Text(account?.label ?? "Account unavailable")
                            Text(account?.canInfer == true ? "Authorized inference scope available" : "Reconnect this account in Models to check access.")
                            Link("Manage ChatGPT usage", destination: URL(string: "https://chatgpt.com/settings/usage")!)
                        }
                    }
                }
                Section("Machine access") {
                    if host.client != nil {
                        Text(host.host?.name ?? host.currentAddress).font(.headline)
                        Text(host.currentAddress).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        Toggle("Allow agent management", isOn: $conversation.allowManagement).disabled(conversation.running)
                        Text("When enabled, this model can create sessions, send prompts, and interrupt agents on this machine. Existing agent permissions still apply.").font(.caption).foregroundStyle(.secondary)
                    } else { Text("Chat works without a machine. Connect one from Machines to inspect or manage its agents.") }
                }
                Section { Text("Prompts and requested host, session, and file content are sent to the selected model endpoint.").font(.caption).foregroundStyle(.secondary) }
            }.navigationTitle("Chat settings")
                .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { showContext = false } } }
        }
        #if os(macOS)
        .frame(minWidth: 400, idealWidth: 480, minHeight: 420)
        #endif
    }
    private func reloadAccounts() { Task { accounts = (try? await ChatGPTAuth.shared.accounts()) ?? [] } }
}
