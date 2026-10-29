#if os(macOS)
import AppKit
import Foundation
import SQLite3
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

/// Direct-distribution macOS launcher. Host processes intentionally outlive windows.
@MainActor final class LocalHostController {
    private var process: Process?
    private let address = "http://127.0.0.1:9847"
    func start(workspace: URL, model: AppModel) async throws {
        let data = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/ControlPlane/Host")
        if let token = CredentialStore.load(host: address), let expected = UserDefaults.standard.string(forKey: "localHostID") {
            let api = try APIClient(address: address, token: token)
            if let info: HostInfo = try? await api.request("v1/host"), info.id == expected {
                try await model.connect(address: address, token: token); return
            }
        }
        // Never enroll against an unknown server already occupying the local port.
        var probe = URLRequest(url: URL(string: address + "/healthz")!); probe.timeoutInterval = 0.15
        if (try? await URLSession.shared.data(for: probe)) != nil { throw APIError.credentials("Port 9847 is already serving an unknown local host. Stop it or connect to it explicitly before starting this app’s host.") }
        let resources = Bundle.main.resourceURL
        let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        let paths = [ProcessInfo.processInfo.environment["CONTROL_PLANE_HOST_BINARY"], resources?.appendingPathComponent("bin/control-plane-host").path, cwd.appendingPathComponent("target/debug/control-plane-host").path, cwd.deletingLastPathComponent().appendingPathComponent("target/debug/control-plane-host").path].compactMap { $0 }
        guard let binary = paths.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) else { throw APIError.credentials("Host binary not found. Use a packaged Mac app, build cargo build -p control-plane-host from the repository, or set CONTROL_PLANE_HOST_BINARY.") }
        try FileManager.default.createDirectory(at: data, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let log = data.appendingPathComponent("launcher.log")
        FileManager.default.createFile(atPath: log.path, contents: nil, attributes: [.posixPermissions: 0o600])
        let logHandle = try FileHandle(forWritingTo: log)
        let child = Process(); child.executableURL = URL(fileURLWithPath: binary)
        child.arguments = ["--data", data.path, "serve", "--workspace-root", workspace.path, "--name", Host.current().localizedName ?? "This Mac"]
        child.standardInput = FileHandle.nullDevice; child.standardOutput = logHandle; child.standardError = logHandle
        var environment = ProcessInfo.processInfo.environment
        environment["CONTROL_PLANE_URL"] = address
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        environment["PATH"] = [resources?.appendingPathComponent("bin").path, home + "/.local/bin", home + "/.cargo/bin", "/opt/homebrew/bin", "/usr/local/bin", environment["PATH"] ?? "/usr/bin:/bin"].compactMap { $0 }.joined(separator: ":")
        let helpers = [resources?.appendingPathComponent("bin/control-plane-desktop").path, resources?.appendingPathComponent("bin/DesktopHelper").path, cwd.appendingPathComponent("helpers/macos/build/control-plane-desktop").path].compactMap { $0 }
        if let helper = helpers.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) { environment["CONTROL_PLANE_DESKTOP_HELPER"] = helper }
        if let sidecar = resources?.appendingPathComponent("claude/src/index.mjs"), FileManager.default.fileExists(atPath: sidecar.path) { environment["CONTROL_PLANE_CLAUDE_SIDECAR"] = sidecar.path }
        child.environment = environment
        try child.run(); process = child
        var ready = false
        let deadline = Date().addingTimeInterval(4.5)
        while Date() < deadline {
            try await Task.sleep(nanoseconds: 100_000_000)
            if !child.isRunning { break }
            if (try? await URLSession.shared.data(for: probe)) != nil { ready = true; break }
        }
        // Give both listeners a chance to bind. A conflicting listener must cause
        // the child to exit before any enrollment secret is sent over the socket.
        try await Task.sleep(nanoseconds: 300_000_000)
        guard ready, child.isRunning else {
            let detail = (try? String(contentsOf: log, encoding: .utf8)).map { String($0.suffix(4000)) } ?? "No diagnostic output"
            throw APIError.credentials("Local host failed to start. \(detail)")
        }
        let enroll = Process(); enroll.executableURL = URL(fileURLWithPath: binary)
        enroll.arguments = ["--data", data.path, "enroll", "--role", "administrator"]
        let output = Pipe(); enroll.standardOutput = output; enroll.standardError = FileHandle.nullDevice
        try enroll.run()
        for _ in 0..<20 { if !enroll.isRunning { break }; try await Task.sleep(nanoseconds: 100_000_000) }
        guard !enroll.isRunning, enroll.terminationStatus == 0, child.isRunning else { if enroll.isRunning { enroll.terminate() }; throw APIError.credentials("Could not create a local device enrollment.") }
        let secret = String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        guard secret.count == 64 else { throw APIError.invalidResponse }
        let expectedID = try hostID(database: data.appendingPathComponent("host.sqlite3"))
        let api = try APIClient(address: address, token: "")
        let paired: PairResult = try await api.request("v1/pair", method: "POST", body: ["secret": .string(secret), "device_name": .string("Control Plane on this Mac")])
        let authenticated = try APIClient(address: address, token: paired.token)
        let info: HostInfo = try await authenticated.request("v1/host")
        guard info.id == expectedID, child.isRunning else { throw APIError.credentials("The local host identity did not match its data directory.") }
        UserDefaults.standard.set(expectedID, forKey: "localHostID")
        try await model.connect(address: address, token: paired.token)
    }
    private func hostID(database: URL) throws -> String {
        var db: OpaquePointer?
        guard sqlite3_open_v2(database.path, &db, SQLITE_OPEN_READONLY, nil) == SQLITE_OK else { throw APIError.credentials("Cannot read local host identity.") }
        defer { sqlite3_close(db) }
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, "SELECT value FROM metadata WHERE key='host_id'", -1, &statement, nil) == SQLITE_OK else { throw APIError.invalidResponse }
        defer { sqlite3_finalize(statement) }
        guard sqlite3_step(statement) == SQLITE_ROW, let bytes = sqlite3_column_text(statement, 0) else { throw APIError.invalidResponse }
        return String(cString: bytes)
    }
}
#endif
