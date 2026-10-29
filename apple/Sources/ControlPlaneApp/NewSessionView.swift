import SwiftUI
#if os(iOS)
import UIKit
#endif
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@MainActor struct NewSessionView: View {
    var onCreated: (String) -> Void = { _ in }
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var provider = "codex"
    @State private var workspace = ""
    @State private var title = ""
    @State private var trusted = false
    @State private var busy = false
    @State private var destinations: [ExecutionDestination] = []
    @State private var destinationID = "native"
    @State private var providers: [ProviderInfo] = []
    @State private var discovering = true
    @State private var refreshID = 0
    @State private var error: String?
    private var executionTarget: JSONValue { destinations.first { $0.id == destinationID }?.execution_target ?? .object(["kind": .string("native")]) }
    private var roots: [String] { destinationID == "native" ? model.host?.workspace_roots ?? [] : destinations.first { $0.id == destinationID }?.roots ?? [] }
    private var canCreate: Bool { !busy && !discovering && !workspace.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && providers.first { $0.name == provider }?.available == true }
    var body: some View {
        NavigationStack {
            Form {
                Section("Agent") {
                    if destinations.contains(where: { $0.id != "native" }) {
                        Picker("Runtime", selection: $destinationID) {
                            Text("Host OS").tag("native")
                            ForEach(destinations.filter { $0.id != "native" }) { Text($0.displayName).tag($0.id) }
                        }
                    }
                    if discovering { ProgressView("Checking installed agents…") }
                    else if providers.contains(where: \.available) {
                        Picker("Coding agent", selection: $provider) {
                            ForEach(providers.filter(\.available)) { Text($0.name.capitalized).tag($0.name) }
                        }
                    } else {
                        Text("No supported agent found").font(.headline)
                        Text("Install Codex, Claude, or Kiro in this runtime, then check again.").foregroundStyle(.secondary)
                    }
                    Button("Check again") { refreshID += 1 }.disabled(discovering)
                }
                Section {
                    if !roots.isEmpty {
                        Menu("Choose a shared folder") {
                            ForEach(roots, id: \.self) { root in Button(root) { workspace = root } }
                        }
                    }
                    TextField("Absolute folder path", text: $workspace).autocorrectionDisabled().accessibilityIdentifier("session.workspace")
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        #endif
                    TextField("Session name (optional)", text: $title)
                } header: { Text("Workspace") } footer: {
                    Text(destinationID == "native" ? "Choose a folder on \(model.host?.name ?? "this machine")." : "Files, Git, and the terminal use this WSL distribution. Desktop control stays on Windows.")
                }
                Section {
                    Toggle("Trusted access", isOn: $trusted)
                } header: { Text("Permissions") } footer: {
                    Text("Grants the agent and terminal this machine account's full access. Other sessions keep the provider's normal approvals.")
                }
            }
            .navigationTitle("New session")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            .scrollDismissesKeyboard(.interactively)
            #endif
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() }.accessibilityIdentifier("session.create.cancel") } }
            .safeAreaInset(edge: .bottom) {
                VStack(alignment: .leading, spacing: 12) {
                    if let error { Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true) }
                    Button { create() } label: { Text(busy ? "Creating…" : "Create session").frame(maxWidth: .infinity, minHeight: 28) }
                    .buttonStyle(.borderedProminent).controlSize(.large)
                    .disabled(!canCreate)
                }.padding().background(.bar)
            }
            .task {
                do {
                    guard let client = model.client else { return }
                    destinations = try await client.request("v1/execution-targets")
                } catch { self.error = error.localizedDescription }
            }
            .task(id: destinationID + String(refreshID)) {
                discovering = true; providers = []
                do {
                    guard let client = model.client else { discovering = false; return }
                    let result: [ProviderInfo] = try await client.request("v1/providers/discover", method: "POST", body: ["execution_target": executionTarget])
                    try Task.checkCancellation()
                    providers = result
                    if result.first(where: { $0.name == provider })?.available != true, let first = result.first(where: \.available) { provider = first.name }
                    discovering = false
                } catch is CancellationError {} catch { discovering = false; self.error = error.localizedDescription }
            }
            .onAppear { if workspace.isEmpty { workspace = roots.first ?? "" } }
            .onChange(of: roots) { _, values in if workspace.isEmpty { workspace = values.first ?? "" } }
            .onChange(of: destinationID) { _, _ in workspace = roots.first ?? "" }
        }
        #if os(macOS)
        .frame(minWidth: 400, idealWidth: 480, minHeight: 460)
        #endif
    }
    private func create() {
        #if os(iOS)
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        #endif
        busy = true; error = nil
        Task {
            defer { busy = false }
            do {
                try await model.create(provider: provider, workspace: workspace.trimmingCharacters(in: .whitespacesAndNewlines), title: title, trusted: trusted, executionTarget: executionTarget)
                if let id = model.selectedID { onCreated(id) }
                dismiss()
            } catch { self.error = error.localizedDescription }
        }
    }
}
