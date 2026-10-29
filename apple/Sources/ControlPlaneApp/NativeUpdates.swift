import Foundation
import SwiftUI

#if os(macOS)
  private final class UpdateProcessBox: @unchecked Sendable {
    private let lock = NSLock()
    private var process: Process?
    private var cancelled = false
    func start(_ p: Process) throws {
      lock.lock()
      defer { lock.unlock() }
      if cancelled { throw CancellationError() }
      process = p
      try p.run()
    }
    func stop() {
      lock.lock()
      cancelled = true
      let p = process
      lock.unlock()
      if let p, p.isRunning { p.terminate() }
    }
  }
  private final class UpdateOutput: @unchecked Sendable {
    private let lock = NSLock()
    private var bytes = Data()
    private var exceeded = false
    func read(_ h: FileHandle, limit: Int, box: UpdateProcessBox) {
      while true {
        let b = h.readData(ofLength: 8192)
        if b.isEmpty { break }
        lock.lock()
        if bytes.count + b.count > limit {
          exceeded = true
          lock.unlock()
          box.stop()
          break
        }
        bytes.append(b)
        lock.unlock()
      }
    }
    func value() throws -> Data {
      lock.lock()
      defer { lock.unlock() }
      if exceeded {
        throw NSError(
          domain: "Update", code: 1,
          userInfo: [NSLocalizedDescriptionKey: "Updater output exceeded limit."])
      }
      return bytes
    }
  }
  private func runUpdater(_ args: [String]) async throws -> Data {
    let candidates = [
      Bundle.main.resourceURL?.appendingPathComponent("bin/control-plane-update"),
      Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent(
        "control-plane-update"),
    ].compactMap { $0 }
    guard
      let executable = candidates.first(where: {
        FileManager.default.isExecutableFile(atPath: $0.path)
      })
    else {
      throw NSError(
        domain: "Update", code: 2,
        userInfo: [
          NSLocalizedDescriptionKey: "This development build has no bundled publisher updater."
        ])
    }
    let box = UpdateProcessBox()
    return try await withTaskCancellationHandler(
      operation: {
        try await Task.detached(priority: .utility) {
          let p = Process()
          p.executableURL = executable
          p.arguments = args
          p.standardInput = FileHandle.nullDevice
          let out = Pipe()
          let err = Pipe()
          p.standardOutput = out
          p.standardError = err
          let output = UpdateOutput()
          let errors = UpdateOutput()
          let group = DispatchGroup()
          try box.start(p)
          group.enter()
          DispatchQueue.global(qos: .utility).async {
            output.read(out.fileHandleForReading, limit: 131072, box: box)
            group.leave()
          }
          group.enter()
          DispatchQueue.global(qos: .utility).async {
            errors.read(err.fileHandleForReading, limit: 32768, box: box)
            group.leave()
          }
          DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 620) { box.stop() }
          p.waitUntilExit()
          group.wait()
          let data = try output.value()
          let error = try errors.value()
          guard p.terminationStatus == 0 else {
            throw NSError(
              domain: "Update", code: Int(p.terminationStatus),
              userInfo: [
                NSLocalizedDescriptionKey: String(data: error, encoding: .utf8)
                  ?? "Verified update failed."
              ])
          }
          return data
        }.value
      }, onCancel: { box.stop() })
  }
  private struct UpdateInfo: Decodable {
    struct Artifact: Decodable { let size: UInt64 }
    let version: String
    let available: Bool
    let artifact: Artifact
  }
  private struct StagedUpdate: Decodable {
    let verified: Bool
    let version: String
    let artifact: String
  }
  @MainActor final class NativeUpdateModel: ObservableObject {
    @Published private(set) var availableVersion: String?
    @Published private(set) var stagedArtifact: URL?
    @Published private(set) var busy = false
    @Published private(set) var applying = false
    @Published private(set) var status = "Updates use this release’s pinned publisher key."
    @Published private(set) var error: String?
    private var task: Task<Void, Never>?
    private var base: URL {
      FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("ControlPlane", isDirectory: true)
    }
    func check() {
      perform {
        let d = try await runUpdater(["check"])
        try Task.checkCancellation()
        let r = try JSONDecoder().decode(UpdateInfo.self, from: d)
        guard r.available else {
          self.availableVersion = nil
          self.stagedArtifact = nil
          self.status = "You’re up to date."
          return
        }
        self.availableVersion = r.version
        self.stagedArtifact = nil
        self.status =
          "Version \(r.version) is available (\(ByteCountFormatter.string(fromByteCount:Int64(r.artifact.size),countStyle:.file)))."
      }
    }
    func stage() {
      perform {
        let directory = self.base.appendingPathComponent("Updates", isDirectory: true)
        let d = try await runUpdater(["stage", "--directory", directory.path])
        try Task.checkCancellation()
        let r = try JSONDecoder().decode(StagedUpdate.self, from: d)
        let path = URL(fileURLWithPath: r.artifact).resolvingSymlinksInPath()
        guard r.verified, path.deletingLastPathComponent() == directory.resolvingSymlinksInPath(),
          path.pathExtension == "pkg"
        else {
          throw NSError(
            domain: "Update", code: 3,
            userInfo: [NSLocalizedDescriptionKey: "Unexpected staging path."])
        }
        self.stagedArtifact = path
        self.availableVersion = r.version
        self.status = "Signature and artifact hash verified. Ready to prepare the installer."
      }
    }
    func apply() {
      guard let path = stagedArtifact else { return }
      perform {
        self.applying = true
        defer { self.applying = false }
        let worker = self.base.appendingPathComponent("Host", isDirectory: true)
        try FileManager.default.createDirectory(
          at: worker, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        _ = try await runUpdater(["apply", path.path, "--worker-data", worker.path])
        self.status = "Installer opened. Finish installation before starting the local host again."
      }
    }
    var maintenancePending: Bool {
      FileManager.default.fileExists(atPath: base.appendingPathComponent("Host/update.drain").path)
    }
    func resume() {
      perform {
        _ = try await runUpdater([
          "resume", "--worker-data", self.base.appendingPathComponent("Host").path,
        ])
        self.status =
          "Maintenance cleared. You can start the local host; previous work will not be replayed."
      }
    }
    func cancel() {
      guard !applying else { return }
      task?.cancel()
      if busy { status = "Cancelling update operation…" }
    }
    private func perform(_ action: @escaping () async throws -> Void) {
      guard !busy else { return }
      busy = true
      error = nil
      task = Task {
        defer {
          self.busy = false
          self.task = nil
        }
        do {
          try await action()
          try Task.checkCancellation()
        } catch is CancellationError { self.status = "Update operation cancelled." } catch {
          if Task.isCancelled {
            self.status = "Update operation cancelled."
          } else {
            self.error = error.localizedDescription
          }
        }
      }
    }
  }
#endif
@MainActor struct UpdatesView: View {
  #if os(macOS)
    @StateObject private var updater = NativeUpdateModel()
    @State private var confirm = false
  #endif
  var body: some View {
    Form {
      #if os(macOS)
        Section("Application updates") {
          Text(updater.status).textSelection(.enabled)
          Button("Check for updates") { updater.check() }.disabled(updater.busy)
          if updater.availableVersion != nil {
            Button("Download and verify update") { updater.stage() }.disabled(updater.busy)
          }
          if updater.stagedArtifact != nil {
            Button("Prepare verified installer") { confirm = true }.disabled(updater.busy)
          }
          if updater.busy {
            HStack {
              ProgressView()
              if !updater.applying { Button("Cancel") { updater.cancel() } }
            }
          }
          if updater.maintenancePending {
            Button("Resume after a cancelled installation") { updater.resume() }.disabled(
              updater.busy)
          }
          if let error = updater.error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
          Text(
            "Installation drains this app’s local worker. Other execution hosts have their own update commands."
          ).font(.caption).foregroundStyle(.secondary)
        }
      #else
        Section("Application updates") {
          Text(
            "Use App Store or TestFlight to update this iPhone or iPad app. Host updates are managed on each execution machine."
          )
        }
      #endif
    }.formStyle(.grouped).navigationTitle("Updates")
      #if os(macOS)
        .confirmationDialog(
          "Drain the local worker and open the verified installer?", isPresented: $confirm,
          titleVisibility: .visible
        ) { Button("Prepare installer") { updater.apply() } }.onDisappear { updater.cancel() }
      #endif
  }
}
