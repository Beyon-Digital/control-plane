import SwiftUI
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@main struct ControlPlaneApp: App {
    #if os(macOS)
    @NSApplicationDelegateAdaptor(PushApplicationDelegate.self) private var pushDelegate
    #else
    @UIApplicationDelegateAdaptor(PushApplicationDelegate.self) private var pushDelegate
    #endif
    @StateObject private var model = AppModel()
    var body: some Scene {
        WindowGroup {
            #if os(iOS)
            IOSRootView().environmentObject(model).tint(.indigo)
            #else
            RootView().environmentObject(model).tint(.indigo)
            #endif
        }
        #if os(macOS)
        .defaultSize(width: 1180, height: 800)
        #endif
    }
}

@MainActor struct RootView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var orchestrator = OrchestratorModel()
    @ObservedObject private var profiles = ModelProfileStore.shared
    @State private var page = "__orchestrator"
    @State private var showConnect = false
    @State private var showNew = false
    @State private var showHistory = false
    @State private var showTeam = false
    @State private var confirmStop = false
    private var sidebarSelection: Binding<String?> {
        Binding(get: { page == "sessions" ? model.selectedID : page }, set: { value in
            guard let value else { return }
            if value == "__orchestrator" || value == "__models" || value == "__updates" { page = value }
            else { page = "sessions"; model.selectedID = value }
        })
    }
    var body: some View {
        NavigationSplitView {
            List(selection: sidebarSelection) {
                Section("Workspace") {
                    Label("Orchestrator", systemImage: "sparkles").tag("__orchestrator")
                    Label("Models", systemImage: "slider.horizontal.3").tag("__models").accessibilityIdentifier("navigation.models")
                    Label("Updates",systemImage:"arrow.down.circle").tag("__updates")
                }
                Section("Machines") {
                    ForEach(model.savedHosts) { host in
                        Button { model.reconnect(host) } label: {
                            Label(host.name, systemImage: "desktopcomputer").foregroundStyle(model.currentAddress == host.address ? .primary : .secondary)
                        }.buttonStyle(.plain)
                    }
                    Button("Connect a machine", systemImage: "plus.circle") { showConnect = true }.accessibilityIdentifier("host.connect")
                    #if os(macOS)
                    Button(model.connecting ? "Starting local host…" : "Start local host…", systemImage: "desktopcomputer.badge.plus") { model.startLocalHost() }.disabled(model.connecting)
                    #endif
                }
                Section("Agent sessions") {
                    ForEach(model.sessions.filter { $0.state != "archived" }) { session in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(session.title.isEmpty ? "Untitled session" : session.title).lineLimit(1)
                            HStack { Text(session.provider.capitalized); Spacer(); Text(session.state).foregroundStyle(session.state == "running" ? .green : .secondary) }.font(.caption)
                        }.padding(.vertical, 4).tag(session.id)
                    }
                }
            }
            .navigationTitle("Control Plane")
            .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 360)
            .safeAreaInset(edge: .bottom) {
                HStack(spacing: 8) {
                    Circle().fill(model.connection == "Connected" ? .green : .orange).frame(width: 7, height: 7)
                    Text(model.connection).font(.caption).lineLimit(2); Spacer()
                }.padding().background(.bar)
            }
            .toolbar {
                ToolbarItem { Button("New session", systemImage: "square.and.pencil") { page = "sessions"; showNew = true }.disabled(model.host == nil) }
                ToolbarItem { Button("Import history",systemImage:"clock.arrow.circlepath"){showHistory=true}.disabled(model.client==nil) }
                ToolbarItem { Button("Team devices", systemImage: "person.2") { showTeam = true }.disabled(model.host == nil) }
            }
        } detail: {
            if page == "__orchestrator" { OrchestratorView(conversation: orchestrator) }
            else if page == "__models" { ModelsView(onCredentialsChange: orchestrator.credentialsWillChange) }
            else if page == "__updates" { UpdatesView() }
            else if let session = model.selected { SessionView(session: session) }
            else {
                ContentUnavailableView {
                    Label("Your agents, wherever you work", systemImage: "point.3.connected.trianglepath.dotted")
                } description: {
                    Text("Connect to a machine on your private network to start an agent, review changes, or control its desktop.")
                } actions: { Button("Connect a machine") { showConnect = true }.buttonStyle(.borderedProminent).accessibilityIdentifier("host.connect") }
            }
        }
        .sheet(isPresented: $showConnect) { ConnectView().environmentObject(model) }
        .sheet(isPresented:$showHistory){NavigationStack{HistoryImportView(initialWorkspace:"",initialProvider:"codex").toolbar{ToolbarItem(placement:.cancellationAction){Button("Cancel"){showHistory=false}}}}.environmentObject(model)}
        .sheet(isPresented: $showNew) { NewSessionView().environmentObject(model) }
        .sheet(isPresented: $showTeam) { TeamView().environmentObject(model) }
        .alert("Host request failed", isPresented: Binding(get: { model.error != nil }, set: { if !$0 { model.error = nil } })) { Button("OK") { model.error = nil } } message: { Text(model.error ?? "") }
        .toolbar { ToolbarItem(placement: .primaryAction) { Button("Emergency stop", systemImage: "stop.circle.fill", role: .destructive) { confirmStop = true }.disabled(model.host == nil) } }
        .confirmationDialog("Stop all managed agents, terminals, and desktop input on this host?", isPresented: $confirmStop, titleVisibility: .visible) {
            Button("Stop all execution", role: .destructive) { model.perform { try await model.stopAll() } }
        }
        .onReceive(NotificationCenter.default.publisher(for: .controlPlaneNotificationTapped)) { _ in model.perform { try await model.refresh() } }
        .onChange(of: model.currentAddress) { _, address in orchestrator.bind(host: address, profile: profiles.selected) }
        .onChange(of: profiles.selected) { _, profile in orchestrator.bind(host: model.currentAddress, profile: profile) }
        .onChange(of: profiles.revision) { _, _ in orchestrator.credentialsWillChange(); orchestrator.bind(host: model.currentAddress, profile: profiles.selected) }
        .onChange(of: model.connecting) { _, connecting in if connecting { orchestrator.credentialsWillChange() } }
    }
}
