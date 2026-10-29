import SwiftTerm
import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
#if os(macOS)
  struct RemoteTerminalSurface: NSViewRepresentable {
    let terminal: RemoteTerminal
    func makeCoordinator() -> TerminalBridge { TerminalBridge(terminal) }
    func makeNSView(context: Context) -> SwiftTerm.TerminalView {
      let view = SwiftTerm.TerminalView(frame: .zero)
      context.coordinator.bind(view)
      return view
    }
    func updateNSView(_ view: SwiftTerm.TerminalView, context: Context) {}
    static func dismantleNSView(_ view: SwiftTerm.TerminalView, coordinator: TerminalBridge) {
      view.terminalDelegate = nil
    }
  }
#else
  struct RemoteTerminalSurface: UIViewRepresentable {
    let terminal: RemoteTerminal
    func makeCoordinator() -> TerminalBridge { TerminalBridge(terminal) }
    func makeUIView(context: Context) -> SwiftTerm.TerminalView {
      let view = SwiftTerm.TerminalView(frame: .zero)
      context.coordinator.bind(view)
      return view
    }
    func updateUIView(_ view: SwiftTerm.TerminalView, context: Context) {}
    static func dismantleUIView(_ view: SwiftTerm.TerminalView, coordinator: TerminalBridge) {
      view.terminalDelegate = nil
    }
  }
#endif
struct EmulatedTerminalView: View {
  let client: APIClient
  let sessionID: String
  @StateObject private var terminal = RemoteTerminal()
  var body: some View {
    VStack(spacing: 0) {
      HStack {
        Label(terminal.status, systemImage: "terminal").font(.caption).lineLimit(2)
        Spacer()
        Button(terminal.connected ? "Detach" : "Attach") {
          if terminal.connected {
            terminal.stop()
          } else {
            terminal.attach(client: client, sessionID: sessionID)
          }
        }
      }.padding()
      RemoteTerminalSurface(terminal: terminal).frame(maxWidth: .infinity, maxHeight: .infinity)
      HStack {
        Button("Esc") { terminal.send(Data([27])) }
        Button("Tab") { terminal.send(Data([9])) }
        Button("Ctrl-C") { terminal.send(Data([3])) }
        Spacer()
        Button("←") { terminal.send(Data("\u{1b}[D".utf8)) }
        Button("↑") { terminal.send(Data("\u{1b}[A".utf8)) }
        Button("↓") { terminal.send(Data("\u{1b}[B".utf8)) }
        Button("→") { terminal.send(Data("\u{1b}[C".utf8)) }
      }.buttonStyle(.bordered).font(.caption).padding(8).disabled(!terminal.connected)
    }.onDisappear { terminal.stop() }
  }
}
