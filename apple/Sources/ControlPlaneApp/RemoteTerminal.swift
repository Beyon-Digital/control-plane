import SwiftTerm
import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif

@MainActor final class RemoteTerminal: ObservableObject {
  @Published private(set) var connected = false
  @Published private(set) var status = "Detached"
  private var task: Task<Void, Never>?
  private var writer: Task<Void, Never>?
  private var socket: URLSessionWebSocketTask?
  private var input: AsyncStream<[String: JSONValue]>.Continuation?
  private var cursor: UInt64 = 0
  private var columns = 80
  private var rows = 24
  private var pending = Data()
  private var output: ((Data) -> Void)?
  func attach(client: APIClient, sessionID: String) {
    guard task == nil else { return }
    task = Task {
      var backoff: UInt64 = 1
      while !Task.isCancelled {
        do {
          status = "Connecting…"
          let ws = client.socket(
            "v1/terminal", query: ["session_id": sessionID, "after": String(cursor)])
          socket = ws
          ws.maximumMessageSize = 512 * 1024
          ws.resume()
          let (stream, continuation) = AsyncStream<[String: JSONValue]>.makeStream(
            bufferingPolicy: .bufferingOldest(128))
          input = continuation
          writer = Task {
            do {
              for await message in stream {
                try Task.checkCancellation()
                try await ws.send(.data(JSONEncoder().encode(message)))
              }
            } catch {
              if !Task.isCancelled {
                status = "Input connection failed: \(error.localizedDescription)"
                ws.cancel(with: .goingAway, reason: nil)
              }
            }
          }
          connected = true
          status = "Connected"
          backoff = 1
          resize(columns: columns, rows: rows)
          while !Task.isCancelled {
            let message = try await ws.receive()
            let bytes: Data
            switch message {
            case .data(let data): bytes = data
            case .string(let text): bytes = Data(text.utf8)
            @unknown default: continue
            }
            let event = try JSONDecoder().decode(TerminalWireMessage.self, from: bytes)
            if event.type == "error" || event.error != nil {
              throw APIError.credentials(event.error ?? event.message ?? "Terminal request failed")
            }
            if event.type == "exit" {
              status = "Process exited (\(event.code.map(String.init) ?? "unknown"))"
              connected = false
              input?.finish()
              writer?.cancel()
              task = nil
              return
            }
            if event.type == "gap" {
              if let sequence = event.sequence, sequence >= cursor { cursor = sequence }
              deliver(
                Data(
                  "\u{1b}c\r\n[Terminal replay gap: earlier output expired. Screen state was reset.]\r\n"
                    .utf8))
              continue
            }
            guard event.type == "output" else { continue }
            let payload = try event.outputBytes()
            guard let sequence = event.sequence else { throw APIError.invalidResponse }
            guard sequence > cursor else { continue }
            deliver(payload)
            cursor = sequence
          }
        } catch {
          connected = false
          input?.finish()
          input = nil
          writer?.cancel()
          socket?.cancel(with: .goingAway, reason: nil)
          guard !Task.isCancelled else { return }
          status = "Reconnecting · \(error.localizedDescription)"
          try? await Task.sleep(nanoseconds: backoff * 1_000_000_000)
          backoff = min(backoff * 2, 15)
        }
      }
    }
  }
  private func deliver(_ data: Data) {
    if let output {
      output(data)
    } else {
      pending.append(data)
      if pending.count > 256 * 1024 { pending = Data(pending.suffix(256 * 1024)) }
    }
  }
  func bind(_ handler: @escaping (Data) -> Void) {
    output = handler
    if !pending.isEmpty {
      handler(pending)
      pending.removeAll(keepingCapacity: false)
    }
  }
  private func enqueue(_ message: [String: JSONValue]) {
    guard let input else { return }
    if case .dropped = input.yield(message) {
      status = "Terminal input queue is full; disconnected to preserve input order."
      socket?.cancel(with: .goingAway, reason: nil)
      input.finish()
    }
  }
  func send(_ data: Data) {
    guard connected else { return }
    guard data.count <= 1024 * 1024 else {
      status = "Paste exceeds 1 MiB; input was not sent."
      return
    }
    for start in stride(from: 0, to: data.count, by: 16384) {
      let chunk = data.subdata(in: start..<min(start + 16384, data.count))
      enqueue(["type": .string("input"), "data": .string(chunk.base64EncodedString())])
    }
  }
  func resize(columns: Int, rows: Int) {
    self.columns = max(2, min(columns, 500))
    self.rows = max(2, min(rows, 300))
    if connected {
      enqueue([
        "type": .string("resize"), "cols": .number(Double(self.columns)),
        "rows": .number(Double(self.rows)),
      ])
    }
  }
  func stop() {
    task?.cancel()
    task = nil
    input?.finish()
    input = nil
    writer?.cancel()
    writer = nil
    socket?.cancel(with: .goingAway, reason: nil)
    socket = nil
    connected = false
    status = "Detached"
    cursor = 0
    pending.removeAll()
    deliver(Data("\u{1b}c".utf8))
  }
}

@MainActor final class TerminalBridge: NSObject, @preconcurrency SwiftTerm.TerminalViewDelegate {
  let terminal: RemoteTerminal
  init(_ terminal: RemoteTerminal) { self.terminal = terminal }
  func bind(_ view: SwiftTerm.TerminalView) {
    view.terminalDelegate = self
    view.changeScrollback(2000)
    #if os(macOS)
      view.configureNativeColors()
    #endif
    terminal.bind { [weak view] data in view?.feed(byteArray: Array(data)[...]) }
  }
  func sizeChanged(source: SwiftTerm.TerminalView, newCols: Int, newRows: Int) {
    terminal.resize(columns: newCols, rows: newRows)
  }
  func send(source: SwiftTerm.TerminalView, data: ArraySlice<UInt8>) { terminal.send(Data(data)) }
  func setTerminalTitle(source: SwiftTerm.TerminalView, title: String) {}
  func hostCurrentDirectoryUpdate(source: SwiftTerm.TerminalView, directory: String?) {}
  func scrolled(source: SwiftTerm.TerminalView, position: Double) {}
  func requestOpenLink(source: SwiftTerm.TerminalView, link: String, params: [String: String]) {}
  func bell(source: SwiftTerm.TerminalView) {}
  func clipboardCopy(source: SwiftTerm.TerminalView, content: Data) {}
  func clipboardRead(source: SwiftTerm.TerminalView) -> Data? { nil }
  func iTermContent(source: SwiftTerm.TerminalView, content: ArraySlice<UInt8>) {}
  func rangeChanged(source: SwiftTerm.TerminalView, startY: Int, endY: Int) {}
}
