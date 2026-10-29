import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
private struct ExternalHistory: Decodable, Identifiable {
  let id: String
  let title: String?
  let can_import: Bool?
  let detail: String?
}
private struct ExternalHistoryList: Decodable { let sessions: [ExternalHistory] }
struct HistoryImportView: View {
  @EnvironmentObject private var model: AppModel
  @Environment(\.dismiss) private var dismiss
  let initialWorkspace: String, initialProvider: String
  var initialExecutionTarget: JSONValue = .object(["kind": .string("native")])
  @State private var workspace = ""
  @State private var provider = "codex"
  @State private var targetID = "native"
  @State private var status = ""
  @State private var targets: [ExecutionDestination] = []
  @State private var entries: [ExternalHistory] = []
  @State private var loading = false
  @State private var importing: String?
  private var target: JSONValue {
    targets.first { $0.id == targetID }?.execution_target ?? initialExecutionTarget
  }
  var body: some View {
    Form {
      Section("Provider history") {
        Picker("Provider", selection: $provider) {
          ForEach(["codex", "claude", "kiro"], id: \.self) { Text($0.capitalized).tag($0) }
        }
        Picker("Execution target", selection: $targetID) {
          Text("Host OS").tag("native")
          ForEach(targets.filter { $0.id != "native" }) { Text($0.displayName).tag($0.id) }
        }
        TextField("Absolute workspace path on host", text: $workspace).autocorrectionDisabled()
        Button("Find available history") {
          let p = provider
          let w = workspace
          let t = target
          loading = true
          status = ""
          model.perform {
            defer { loading = false }
            guard let client = model.client else { return }
            let response: ExternalHistoryList = try await client.request(
              "v1/history",
              query: ["provider": p, "workspace": w, "execution_target": t.prettyPrinted])
            entries = response.sessions
            if entries.isEmpty { status = "No importable history for this provider and workspace." }
          }
        }.disabled(workspace.isEmpty)
        Text(
          "Import reads provider session metadata. Imported sessions use supervised permissions; another running process is never taken over."
        ).font(.caption).foregroundStyle(.secondary)
      }.disabled(loading || importing != nil)
      if loading { ProgressView("Reading provider history…") }
      if !status.isEmpty { Text(status).font(.caption) }
      Section("Sessions") {
        ForEach(entries) { entry in
          VStack(alignment: .leading) {
            Text(entry.title ?? entry.id).font(.headline)
            Text(entry.id).font(.caption.monospaced())
            if let detail = entry.detail { Text(detail).font(.caption) }
            Button(importing == entry.id ? "Importing…" : "Import session") {
              let p = provider
              let w = workspace
              let t = target
              importing = entry.id
              model.perform {
                defer { importing = nil }
                guard let client = model.client else { return }
                let session: AgentSession = try await client.request(
                  "v1/history/import", method: "POST",
                  body: [
                    "provider": .string(p), "workspace": .string(w),
                    "provider_session_id": .string(entry.id),
                    "title": .string(entry.title ?? "Imported \(p.capitalized) session"),
                    "trusted": .bool(false), "request_id": .string(UUID().uuidString),
                    "execution_target": t,
                  ])
                try await model.refresh()
                model.selectedID = session.id
                dismiss()
              }
            }.disabled(entry.can_import != true || importing != nil || loading)
          }
        }
      }
    }.navigationTitle("Import agent history").task {
      workspace = initialWorkspace
      provider = initialProvider
      targetID =
        initialExecutionTarget["kind"]?.text == "wsl"
        ? "wsl:" + (initialExecutionTarget["distribution"]?.text ?? "") : "native"
      model.perform {
        guard let client = model.client else { return }
        targets = try await client.request("v1/execution-targets")
      }
    }
    .onChange(of: provider) { _, _ in
      entries = []
      status = ""
    }.onChange(of: workspace) { _, _ in
      entries = []
      status = ""
    }.onChange(of: targetID) { _, _ in
      entries = []
      status = ""
    }
  }
}
