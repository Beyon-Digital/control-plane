#if os(iOS)
import SwiftUI
import UIKit
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@MainActor struct IOSRootView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var orchestrator = OrchestratorModel()
    @State private var tab = WorkspaceTab.agents
    @State private var sessionPath: [String] = []
    @State private var sheet: WorkspaceSheet?
    @State private var confirmStop = false
    private enum WorkspaceTab: Hashable { case agents, chat, machines, settings }
    private enum WorkspaceSheet: String, Identifiable {
        case connect, newSession, history, team
        var id: String { rawValue }
    }
    private var sessions: [AgentSession] { model.sessions.filter { $0.state != "archived" } }

    var body: some View {
        TabView(selection: $tab) {
            agents.tabItem { Label("Agents", systemImage: "terminal").accessibilityIdentifier("nav.agents") }.tag(WorkspaceTab.agents)
            NavigationStack { OrchestratorView(conversation: orchestrator) }
                .tabItem { Label("Chat", systemImage: "bubble.left.and.bubble.right").accessibilityIdentifier("nav.chat") }.tag(WorkspaceTab.chat)
            machines.tabItem { Label("Machines", systemImage: "desktopcomputer").accessibilityIdentifier("nav.machines") }.tag(WorkspaceTab.machines)
            settings.tabItem { Label("Settings", systemImage: "gearshape").accessibilityIdentifier("nav.settings") }.tag(WorkspaceTab.settings)
        }
        .sheet(item: $sheet) { item in
            switch item {
            case .connect: ConnectView()
            case .newSession:
                NewSessionView(onCreated: { id in sheet = nil; tab = .agents; sessionPath = [id] })
            case .history:
                NavigationStack {
                    HistoryImportView(initialWorkspace: "", initialProvider: "codex")
                        .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { sheet = nil } } }
                }
            case .team: TeamView()
            }
        }
        .alert("Unable to complete request", isPresented: Binding(get: { model.error != nil }, set: { if !$0 { model.error = nil } })) {
            Button("OK", role: .cancel) { model.error = nil }
        } message: { Text(model.error ?? "") }
        .confirmationDialog("Stop all execution on this machine?", isPresented: $confirmStop, titleVisibility: .visible) {
            Button("Emergency stop", role: .destructive) { model.perform { try await model.stopAll() } }
        } message: { Text("Running agents, terminals, and remote desktop input will stop.") }
        .onChange(of: model.connecting) { _, connecting in if connecting { orchestrator.credentialsWillChange() } }
        .onChange(of: model.currentAddress) { _, _ in sessionPath = []; if model.host != nil { tab = .agents } }
        .onReceive(NotificationCenter.default.publisher(for: .controlPlaneNotificationTapped)) { _ in model.perform { try await model.refresh() } }
    }

    private var agents: some View {
        NavigationStack(path: $sessionPath) {
            List {
                if let host = model.host {
                    Section {
                        HStack {
                            VStack(alignment: .leading, spacing: 5) {
                                Text(host.name).font(.headline)
                                Text(model.connection).font(.subheadline).foregroundStyle(.secondary)
                            }
                            Spacer()
                            if model.connecting { ProgressView() }
                            else { Image(systemName: model.connection.hasPrefix("Connected") ? "checkmark.circle.fill" : "arrow.trianglehead.2.clockwise").foregroundStyle(model.connection.hasPrefix("Connected") ? .green : .orange) }
                        }.padding(.vertical, 5)
                    }
                    Section("Sessions") {
                        if sessions.isEmpty {
                            VStack(alignment: .leading, spacing: 12) {
                                Text("Ready to start").font(.headline)
                                Text("Choose an agent and a workspace on this machine.").foregroundStyle(.secondary)
                                Button("New session") { sheet = .newSession }.buttonStyle(.borderedProminent).controlSize(.large)
                            }.padding(.vertical, 10)
                        } else {
                            ForEach(sessions) { session in NavigationLink(value: session.id) { IOSSessionRow(session: session) }.accessibilityIdentifier("session.open." + session.id) }
                        }
                    }
                } else {
                    Section {
                        VStack(alignment: .leading, spacing: 16) {
                            Image(systemName: "terminal").font(.largeTitle).foregroundStyle(.indigo).padding(.bottom, 4)
                            Text("Your agents, wherever you work").font(.title2.weight(.semibold))
                            Text("Connect a machine to start a coding session, review changes, or use its desktop.").foregroundStyle(.secondary)
                            Button("Connect a machine") { sheet = .connect }
                                .buttonStyle(.borderedProminent).controlSize(.large).accessibilityIdentifier("host.connect").disabled(model.connecting)
                            if model.connecting { ProgressView("Connecting…") }
                        }.padding(.vertical, 16)
                    }
                    if !model.savedHosts.isEmpty {
                        Section("Recent machines") {
                            ForEach(model.savedHosts) { saved in Button(saved.name) { model.reconnect(saved) }.frame(minHeight: 44).disabled(model.connecting) }
                        }
                    }
                }
            }
            .listStyle(.insetGrouped)
            .navigationTitle("Agents")
            .refreshable { do { try await model.refresh() } catch { model.error = error.localizedDescription } }
            .toolbar {
                if model.host != nil {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button("New session", systemImage: "plus") { sheet = .newSession }.accessibilityIdentifier("agents.new-session")
                    }
                    ToolbarItem(placement: .topBarTrailing) { machineActions }
                }
            }
            .navigationDestination(for: String.self) { id in
                Group {
                    if let session = model.sessions.first(where: { $0.id == id }) { SessionView(session: session).onAppear { model.selectedID = id } }
                    else { ContentUnavailableView("Session unavailable", systemImage: "terminal", description: Text("Return to Agents and refresh the session list.")) }
                }.toolbar(.hidden, for: .tabBar)
            }
        }
    }

    private var machineActions: some View {
        Menu {
            Button("Import history", systemImage: "clock.arrow.circlepath") { sheet = .history }
            Button("Team and devices", systemImage: "person.2") { sheet = .team }
            Divider()
            Button("Emergency stop", systemImage: "stop.circle", role: .destructive) { confirmStop = true }
        } label: { Image(systemName: "ellipsis") }.accessibilityLabel("Machine actions")
    }

    private var machines: some View {
        NavigationStack {
            List {
                if model.savedHosts.isEmpty {
                    Section {
                        VStack(alignment: .leading, spacing: 12) {
                            Text("No machines yet").font(.headline)
                            Text("Add the address and pairing code from your host.").foregroundStyle(.secondary)
                            Button("Add a machine") { sheet = .connect }.buttonStyle(.borderedProminent).controlSize(.large)
                        }.padding(.vertical, 10)
                    }
                } else {
                    Section("Saved machines") {
                        ForEach(model.savedHosts) { saved in
                            Button { if saved.address == model.currentAddress && model.host != nil { tab = .agents } else { model.reconnect(saved) } } label: {
                                HStack {
                                    VStack(alignment: .leading, spacing: 6) {
                                        Text(saved.name).font(.headline).foregroundStyle(.primary)
                                        Text(saved.address).font(.caption).foregroundStyle(.secondary).lineLimit(2).truncationMode(.middle)
                                    }
                                    Spacer()
                                    if saved.address == model.currentAddress && model.host != nil { Image(systemName: "checkmark.circle.fill").foregroundStyle(.indigo).accessibilityLabel("Selected machine") }
                                }.frame(minHeight: 48)
                            }.disabled(model.connecting)
                        }
                    }
                }
                if model.connecting { Section { ProgressView("Connecting…") } }
                if let host = model.host { Section(host.name) { Button("Team and devices") { sheet = .team }; Button("Emergency stop", role: .destructive) { confirmStop = true } } }
            }
            .listStyle(.insetGrouped).navigationTitle("Machines")
            .toolbar { ToolbarItem(placement: .topBarTrailing) { Button("Add machine", systemImage: "plus") { sheet = .connect }.accessibilityIdentifier("machines.add") } }
        }
    }

    private var settings: some View {
        NavigationStack {
            Form {
                Section("On this device") {
                    NavigationLink { ModelsView(onCredentialsChange: orchestrator.credentialsWillChange) } label: { Text("Models and accounts") }.accessibilityIdentifier("settings.models")
                }
                if let host = model.host {
                    Section(host.name) {
                        Button("Team and devices") { sheet = .team }
                        NavigationLink("Device security") { DeviceSecurityView() }
                        NavigationLink("Resources and retention") { ResourcePolicyView() }
                    }
                }
                Section("About") { LabeledContent("Version", value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "Development") }
            }.navigationTitle("Settings")
        }
    }
}

private struct IOSSessionRow: View {
    @Environment(\.dynamicTypeSize) private var typeSize
    let session: AgentSession
    private var workspace: String { session.workspace.replacingOccurrences(of: "\\", with: "/").split(separator: "/").last.map(String.init) ?? session.workspace }
    private var status: String { session.state.replacingOccurrences(of: "_", with: " ").capitalized }
    private var color: Color { session.state == "running" ? .indigo : session.state.contains("approval") ? .orange : session.state == "error" || session.state == "failed" ? .red : .secondary }
    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(session.title.isEmpty ? session.provider.capitalized : session.title).font(.body.weight(.semibold)).lineLimit(2)
            if typeSize.isAccessibilitySize {
                Text("\(session.provider.capitalized) · \(workspace)").font(.subheadline).foregroundStyle(.secondary)
                Text(status).font(.caption.weight(.medium)).foregroundStyle(color)
            } else {
                HStack {
                    Text("\(session.provider.capitalized) · \(workspace)").font(.subheadline).foregroundStyle(.secondary).lineLimit(1)
                    Spacer(minLength: 8)
                    Text(status).font(.caption.weight(.medium)).foregroundStyle(color)
                }
            }
        }.padding(.vertical, 5).accessibilityElement(children: .combine)
    }
}
#endif
