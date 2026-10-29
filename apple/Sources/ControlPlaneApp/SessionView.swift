import SwiftUI
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

struct SessionView: View {
    @EnvironmentObject private var model: AppModel
    let session: AgentSession
    @State private var tab = "Conversation"
    @State private var prompt = ""
    @State private var sending = false
    @State private var showTools = false
    var body: some View {
        VStack(spacing: 0) {
            #if os(iOS)
            PhoneSessionHeader(workspace: session.workspace, state: session.state, selection: $tab)
            #else
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text(session.title.isEmpty ? session.provider.capitalized : session.title).font(.title2.bold())
                    Label(session.workspace, systemImage: "folder").font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                }
                Spacer()
                Text(session.state.capitalized).font(.caption.weight(.medium)).padding(.horizontal, 10).padding(.vertical, 5).background(.quaternary, in: Capsule())
            }.padding()
            Picker("Panel", selection: $tab) {
                ForEach(["Conversation", "Files", "Changes", "Terminal", "Desktop"], id: \.self) { Text($0) }
            }.pickerStyle(.segmented).padding([.horizontal, .bottom])
            #endif
            Divider()
            switch tab {
            case "Files": WorkspaceView(sessionID: session.id)
            case "Changes": DiffView(sessionID: session.id)
            case "Terminal":
                if session.trusted == false { ContentUnavailableView("Trusted session required", systemImage: "lock.shield", description: Text("Create a session with trusted access to attach an interactive terminal.")) }
                else if let client = model.client { EmulatedTerminalView(client: client, sessionID: session.id).id(session.id) }
            case "Desktop": DesktopView()
            default: conversation
            }
        }
        #if os(iOS)
        .navigationTitle(session.title.isEmpty ? session.provider.capitalized : session.title)
        .navigationBarTitleDisplayMode(.inline)
        #else
        .navigationTitle(session.provider.capitalized)
        #endif
        .toolbar {
            #if os(iOS)
            ToolbarItem(placement: .topBarTrailing) {
                Menu {
                    Button("Session tools", systemImage: "wrench.and.screwdriver") { showTools = true }
                    Button("Refresh", systemImage: "arrow.clockwise") { model.perform { try await model.refresh() } }
                    Button("Interrupt agent", systemImage: "pause.circle") { model.perform { try await model.action("interrupt") } }
                    Divider()
                    Button("Archive session", systemImage: "archivebox") { model.perform { try await model.action("archive") } }
                } label: { Image(systemName: "ellipsis") }.accessibilityLabel("Session actions")
            }
            #else
            ToolbarItem { Button("Interrupt", systemImage: "pause.circle") { model.perform { try await model.action("interrupt") } } }
            ToolbarItem { Button("Refresh", systemImage: "arrow.clockwise") { model.perform { try await model.refresh() } } }
            ToolbarItem { Button("Session tools", systemImage: "wrench.and.screwdriver") { showTools = true } }
            ToolbarItem { Menu {
                Button("Archive session", systemImage: "archivebox") { model.perform { try await model.action("archive") } }
            } label: { Image(systemName: "ellipsis.circle") } }
            #endif
        }
        .sheet(isPresented: $showTools) { WorkspaceToolsView(session: session).environmentObject(model) }
        .id(session.id)
    }
    private var conversation: some View {
        VStack(spacing: 0) {
            ScrollViewReader { reader in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 16) {
                        if model.events.isEmpty || model.events.allSatisfy({ $0.kind == "state" && $0.payload["state"]?.text == "idle" }) {
                            ContentUnavailableView("What should \(session.provider.capitalized) work on?", systemImage: "text.bubble", description: Text("Send an instruction for this workspace."))
                        } else {
                            ForEach(model.timeline) { entry in EventRow(event: entry.event, text: entry.text).id(entry.id) }
                        }
                    }.padding()
                }
                .onChange(of: model.events.count) { _, _ in if let last = model.timeline.last { reader.scrollTo(last.id, anchor: .bottom) } }
            }
            Divider()
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    TextField("Message \(session.provider.capitalized)…", text: $prompt, axis: .vertical).lineLimit(1...6).textFieldStyle(.plain)
                        .padding(12).background(.quaternary, in: RoundedRectangle(cornerRadius: 16)).accessibilityIdentifier("session.prompt")
                    Button {
                        let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines); sending = true
                        model.perform { defer { sending = false }; try await model.send(text); prompt = "" }
                    } label: {
                        if sending { ProgressView().frame(width: 24, height: 24) }
                        else { Image(systemName: "arrow.up").fontWeight(.semibold).frame(width: 24, height: 24) }
                    }.buttonStyle(.borderedProminent).buttonBorderShape(.circle).controlSize(.large)
                        .accessibilityLabel("Send instruction").disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || sending)
                }
                Text("\(session.provider.capitalized) · \(model.host?.name ?? "Remote machine")").font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }.padding().background(.bar)
        }
    }
}

#if os(iOS)
private struct PhoneSessionHeader: View {
    @Environment(\.dynamicTypeSize) private var typeSize
    let workspace: String
    let state: String
    @Binding var selection: String
    private let panels = ["Conversation", "Files", "Changes", "Terminal", "Desktop"]
    private let symbols = ["bubble.left.and.bubble.right", "folder", "arrow.triangle.branch", "terminal", "desktopcomputer"]
    private var status: String { state.replacingOccurrences(of: "_", with: " ").capitalized }
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if typeSize.isAccessibilitySize {
                VStack(alignment: .leading, spacing: 6) {
                    Text(workspace).lineLimit(2).truncationMode(.middle)
                    Text(status)
                }.font(.caption).foregroundStyle(.secondary)
            } else {
                HStack {
                    Label(workspace, systemImage: "folder").lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 12)
                    Text(status)
                }.font(.caption).foregroundStyle(.secondary)
            }
            Menu {
                Picker("Session panel", selection: $selection) {
                    ForEach(Array(panels.enumerated()), id: \.offset) { index, panel in Label(panel, systemImage: symbols[index]).tag(panel) }
                }.pickerStyle(.inline)
            } label: {
                HStack {
                    Text(selection).font(.subheadline.weight(.semibold))
                    Spacer()
                    Image(systemName: "chevron.down").font(.caption.weight(.semibold))
                }.foregroundStyle(.primary).padding(.horizontal, 14).frame(minHeight: 44)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
            }.buttonStyle(.plain).accessibilityLabel("Session panel").accessibilityValue(selection).accessibilityIdentifier("session.panel")
        }.padding(.horizontal, 16).padding(.vertical, 12)
    }
}
#endif

struct EventRow: View {
    @EnvironmentObject private var model: AppModel
    let event: SessionEvent
    let text: String
    private var isUser: Bool { event.kind == "user_message" }
    private var isMessage: Bool { isUser || event.kind == "message_delta" || event.kind == "assistant" }
    var body: some View {
        if event.kind == "state" {
            VStack(alignment: .leading, spacing: 5) {
                Text((event.payload["state"]?.text ?? text).replacingOccurrences(of: "_", with: " ").capitalized).font(.caption.weight(.medium))
                if let reason = event.payload["reason"]?.text { Text(reason).font(.callout) }
            }.foregroundStyle(event.payload["state"]?.text == "uncertain" ? .orange : .secondary)
        } else if isMessage {
            HStack {
                if isUser { Spacer(minLength: 32) }
                VStack(alignment: .leading, spacing: 8) {
                    if !isUser { Text(model.selected?.provider.capitalized ?? "Agent").font(.caption.weight(.medium)).foregroundStyle(.secondary) }
                    AgentMessageText(text: text)
                }.padding(isUser ? 14 : 0)
                    .background(isUser ? Color.indigo.opacity(0.08) : .clear, in: RoundedRectangle(cornerRadius: 16))
            }
        } else if event.kind == "approval_requested" && !model.approvalResolved(event) {
            VStack(alignment: .leading, spacing: 12) {
                Label("Permission requested", systemImage: "hand.raised").font(.headline).foregroundStyle(.orange)
                Text(text).font(.system(.subheadline, design: .monospaced)).textSelection(.enabled)
                if let request = event.payload["request_id"] ?? event.payload["id"] {
                    HStack {
                        Button("Allow once") { model.perform { try await model.decision(request, decision: "allow") } }
                        Button("Deny", role: .destructive) { model.perform { try await model.decision(request, decision: "deny") } }
                    }.buttonStyle(.bordered).controlSize(.large)
                }
            }.padding(14).background(.quaternary, in: RoundedRectangle(cornerRadius: 14))
        } else if event.kind.contains("error") || event.kind == "history_gap" {
            Label(text, systemImage: "exclamationmark.circle").foregroundStyle(.red).textSelection(.enabled)
        } else {
            DisclosureGroup(event.kind.replacingOccurrences(of: "_", with: " ").capitalized) {
                Text(text).font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding(.top, 6)
                if event.kind == "history_imported", let id = model.selectedID {
                    NavigationLink("Read stored transcript") { SessionHistoryView(sessionID: id) }
                }
            }.font(.subheadline).foregroundStyle(.secondary)
        }
    }
}

struct WorkspaceView: View {
    @EnvironmentObject private var model: AppModel
    let sessionID: String
    @State private var path = ""
    @State private var entries: [FileEntry] = []
    @State private var file: FileContent?
    @State private var content = ""
    @State private var busy = false
    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Button("Parent", systemImage: "arrow.up") { path = (path as NSString).deletingLastPathComponent; load() }.disabled(path.isEmpty)
                Text(path.isEmpty ? "Workspace" : path).font(.caption.monospaced()).lineLimit(1)
                Spacer()
                Button("Refresh", systemImage: "arrow.clockwise") { load() }
            }.padding()
            List(entries) { entry in
                Button { if entry.is_dir { path = entry.path; load() } else { open(entry.path) } } label: {
                    Label(entry.name, systemImage: entry.is_dir ? "folder.fill" : "doc.text").foregroundStyle(entry.is_dir ? .secondary : .primary)
                }.buttonStyle(.plain)
            }
        }.task { load() }
        .sheet(item: Binding(get: { file.map { EditableFile(id: $0.path) } }, set: { if $0 == nil { file = nil } })) { _ in
            NavigationStack {
                TextEditor(text: $content).font(.system(.body, design: .monospaced)).padding(8).navigationTitle(file?.path ?? "File")
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) { Button("Close") { file = nil } }
                        ToolbarItem(placement: .confirmationAction) { Button("Save") { save() }.disabled(busy) }
                    }
            }.frame(minWidth: 340, idealWidth: 700, minHeight: 500)
        }
    }
    private func load() { model.perform { guard let client = model.client else { return }; let result: DirectoryListing = try await client.request("v1/workspace/list", query: ["session_id": sessionID, "path": path]); entries = result.entries } }
    private func open(_ relative: String) { model.perform { guard let client = model.client else { return }; let result: FileContent = try await client.request("v1/workspace/read", query: ["session_id": sessionID, "path": relative]); content = result.content; file = result } }
    private func save() {
        guard let file else { return }; busy = true
        model.perform { defer { busy = false }; guard let client = model.client else { return }; let _: JSONValue = try await client.request("v1/workspace/write", method: "POST", body: ["session_id": .string(sessionID), "path": .string(file.path), "content": .string(content), "expected_sha256": .string(file.sha256)]); self.file = nil; load() }
    }
    private struct EditableFile: Identifiable { let id: String }
}

struct DiffView: View {
    @EnvironmentObject private var model: AppModel
    let sessionID: String
    @State private var output = ""
    @State private var commitMessage = ""
    @State private var confirmPush = false
    var body: some View {
        VStack {
            ScrollView([.horizontal, .vertical]) { Text(output.isEmpty ? "No changes to display." : output).font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding() }
            Divider()
            HStack {
                TextField("Commit message", text: $commitMessage).textFieldStyle(.roundedBorder)
                Button("Commit") { model.perform { try await git("commit", message: commitMessage); commitMessage = ""; load() } }.disabled(commitMessage.isEmpty)
                Button("Push") { confirmPush = true }
                Button("Refresh", systemImage: "arrow.clockwise") { load() }
            }.padding()
        }.task { load() }
        .confirmationDialog("Push this workspace’s branch to its configured remote?", isPresented: $confirmPush, titleVisibility: .visible) { Button("Push branch") { model.perform { try await git("push"); load() } } }
    }
    private func load() { model.perform { guard let client = model.client else { return }; let result: CommandResult = try await client.request("v1/workspace/diff", query: ["session_id": sessionID]); output = result.stdout + result.stderr } }
    private func git(_ operation: String, message: String = "") async throws {
        guard let client = model.client else { return }
        let result: CommandResult = try await client.request("v1/workspace/git", method: "POST", body: ["session_id": .string(sessionID), "operation": .string(operation), "message": .string(message)])
        if !result.success { throw APIError.server(409, result.stderr) }
    }
}
