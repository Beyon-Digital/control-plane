import SwiftUI
import UniformTypeIdentifiers

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
#if os(macOS)
  import AppKit
#else
  import UIKit
#endif
struct WorkspaceToolsView: View {
  @Environment(\.dismiss) private var dismiss
  let session: AgentSession
  var body: some View {
    NavigationStack {
      List {
        NavigationLink {
          WorktreeView(session: session)
        } label: {
          Label("Create Git worktree", systemImage: "arrow.triangle.branch")
        }
        NavigationLink {
          FileTransferView(sessionID: session.id)
        } label: {
          Label("Transfer files", systemImage: "arrow.up.arrow.down")
        }
        NavigationLink {
          HistoryImportView(
            initialWorkspace: session.workspace, initialProvider: session.provider,
            initialExecutionTarget: session.execution_target ?? .object(["kind": .string("native")])
          )
        } label: {
          Label("Import agent history", systemImage: "clock.arrow.circlepath")
        }
        NavigationLink {
          SessionHistoryView(sessionID: session.id)
        } label: {
          Label("Stored provider history", systemImage: "text.book.closed")
        }
        NavigationLink {
          SessionAccessView(sessionID: session.id)
        } label: {
          Label("Session permissions", systemImage: "person.2.badge.key")
        }
      }.navigationTitle("Session tools").toolbar {
        ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
      }
    }
  }
}
struct WorktreeView: View {
  @EnvironmentObject private var model: AppModel
  let session: AgentSession
  @State private var path = ""
  @State private var branch = ""
  @State private var result = ""
  @State private var createdBranch = ""
  @State private var createdPath: String?
  @State private var busy = false
  var body: some View {
    Form {
      Section("New isolated checkout") {
        TextField("Worktree path relative to this workspace", text: $path).autocorrectionDisabled()
        TextField("New branch name", text: $branch).autocorrectionDisabled()
        Text(
          "The host checks repository boundaries and creates a new Git worktree from this session’s checkout."
        ).font(.caption).foregroundStyle(.secondary)
        Button("Create worktree") {
          let destination = path
          let newBranch = branch
          busy = true
          model.perform {
            defer { busy = false }
            guard let client = model.client else { return }
            let response: CommandResult = try await client.request(
              "v1/workspace/git", method: "POST",
              body: [
                "session_id": .string(session.id), "operation": .string("worktree"),
                "path": .string(destination), "branch": .string(newBranch),
              ])
            result = response.stdout + response.stderr
            guard response.success else { throw APIError.credentials(result) }
            createdPath = response.worktree_path
            createdBranch = newBranch
          }
        }.disabled(busy || path.isEmpty || branch.isEmpty)
      }
      if busy { ProgressView("Creating worktree…") }
      if !result.isEmpty { Text(result).font(.caption.monospaced()).textSelection(.enabled) }
      if let createdPath {
        Button("Start a session in this worktree") {
          model.perform {
            try await model.create(
              provider: session.provider, workspace: createdPath, title: createdBranch,
              trusted: false,
              executionTarget: session.execution_target ?? .object(["kind": .string("native")]))
          }
        }
      }
    }.navigationTitle("Git worktree")
  }
}
private struct ExportedFile: Identifiable {
  let url: URL
  var id: String { url.absoluteString }
}
struct FileTransferView: View {
  @EnvironmentObject private var model: AppModel
  let sessionID: String
  @State private var path = ""
  @State private var status = ""
  @State private var replacing = false
  @State private var importing = false
  @State private var busy = false
  @State private var export: ExportedFile?
  @State private var temporary: URL?
  var body: some View {
    Form {
      Section("Remote file") {
        TextField("Path relative to this workspace", text: $path).autocorrectionDisabled()
        Text(
          "Transfers remain inside the selected workspace. Symlink and parent-directory escapes are rejected."
        ).font(.caption).foregroundStyle(.secondary)
      }
      Section("Upload") {
        Toggle("Replace an existing file", isOn: $replacing)
        Text(
          replacing
            ? "A concurrent edit causes a conflict. Replacement verifies the remote SHA-256."
            : "Create a new file; fail if the path already exists."
        ).font(.caption)
        Button("Choose a file to upload", systemImage: "square.and.arrow.up") { importing = true }
          .disabled(path.isEmpty || busy)
      }
      Section("Download") {
        Button("Download and save", systemImage: "square.and.arrow.down") {
          let remote = path
          busy = true
          status = "Downloading…"
          model.perform {
            defer { busy = false }
            guard let client = model.client else { return }
            let url = try await client.downloadFile(sessionID: sessionID, path: remote)
            exportFile(url, filename: (remote as NSString).lastPathComponent)
          }
        }.disabled(path.isEmpty || busy)
      }
      if busy { ProgressView("Transferring…") }
      if !status.isEmpty { Text(status).font(.caption) }
    }.navigationTitle("File transfer").fileImporter(
      isPresented: $importing, allowedContentTypes: [.data], allowsMultipleSelection: false
    ) { result in
      switch result {
      case .success(let urls):
        guard let url = urls.first else { return }
        let access = url.startAccessingSecurityScopedResource()
        let remote = path
        let replace = replacing
        busy = true
        status = "Uploading…"
        model.perform {
          defer {
            busy = false
            if access { url.stopAccessingSecurityScopedResource() }
          }
          guard let client = model.client else { return }
          _ = try await client.uploadFile(
            from: url, sessionID: sessionID, path: remote, replaceExisting: replace)
          status = "Upload complete"
        }
      case .failure(let error): model.error = error.localizedDescription
      }
    }
    #if os(iOS)
      .sheet(item: $export, onDismiss: { cleanup() }) { file in
        ExportDocumentPicker(url: file.url) {
          export = nil
          cleanup()
        }
      }
    #endif
  }
  private func cleanup() {
    if let temporary { try? FileManager.default.removeItem(at: temporary) }
    temporary = nil
  }
  @MainActor private func exportFile(_ url: URL, filename: String) {
    #if os(macOS)
      let panel = NSSavePanel()
      panel.nameFieldStringValue = filename
      panel.canCreateDirectories = true
      defer { try? FileManager.default.removeItem(at: url) }
      guard panel.runModal() == .OK, let destination = panel.url else { return }
      let access = destination.startAccessingSecurityScopedResource()
      defer { if access { destination.stopAccessingSecurityScopedResource() } }
      let stage = destination.deletingLastPathComponent().appendingPathComponent(
        ".control-plane-" + UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: stage) }
      do {
        try FileManager.default.copyItem(at: url, to: stage)
        if FileManager.default.fileExists(atPath: destination.path) {
          _ = try FileManager.default.replaceItemAt(destination, withItemAt: stage)
        } else {
          try FileManager.default.moveItem(at: stage, to: destination)
        }
        status = "Saved \(destination.lastPathComponent)"
      } catch { model.error = error.localizedDescription }
    #else
      temporary = url
      export = ExportedFile(url: url)
    #endif
  }
}
#if os(iOS)
  private struct ExportDocumentPicker: UIViewControllerRepresentable {
    let url: URL
    let finished: () -> Void
    func makeCoordinator() -> Coordinator { Coordinator(finished) }
    func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
      let controller = UIDocumentPickerViewController(forExporting: [url], asCopy: true)
      controller.delegate = context.coordinator
      return controller
    }
    func updateUIViewController(_ controller: UIDocumentPickerViewController, context: Context) {}
    final class Coordinator: NSObject, UIDocumentPickerDelegate {
      let finished: () -> Void
      init(_ finished: @escaping () -> Void) { self.finished = finished }
      func documentPicker(
        _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
      ) { finished() }
      func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) { finished() }
    }
  }
#endif
