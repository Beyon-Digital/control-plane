import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
private struct TranscriptMessage: Decodable {
  let role: String
  let text: String
  let id: String?
  let truncated: Bool?
}
private struct TranscriptPage: Decodable {
  let messages: [TranscriptMessage]
  let next_offset: Int?
  let truncated: Bool
}
struct SessionHistoryView: View {
  @EnvironmentObject private var model: AppModel
  let sessionID: String
  @State private var messages: [TranscriptMessage] = []
  @State private var offset = 0
  @State private var next: Int?
  @State private var busy = false
  @State private var status = ""
  var body: some View {
    ScrollView {
      LazyVStack(alignment: .leading, spacing: 16) {
        ForEach(Array(messages.enumerated()), id: \.offset) { _, message in
          VStack(alignment: .leading, spacing: 6) {
            Text(message.role.capitalized).font(.caption.bold()).foregroundStyle(.secondary)
            Text(message.text).font(
              .system(.body, design: message.role == "tool" ? .monospaced : .default)
            ).textSelection(.enabled)
            if message.truncated == true {
              Text("This source record was truncated to fit the bounded history page.").font(
                .caption
              ).foregroundStyle(.secondary)
            }
          }.padding().frame(maxWidth: .infinity, alignment: .leading).background(
            .quaternary, in: RoundedRectangle(cornerRadius: 10))
        }
        if busy { ProgressView() }
        if !status.isEmpty { Text(status).font(.caption) }
        if next != nil { Button("Load more history") { load(next ?? 0) }.disabled(busy) }
      }.padding()
    }.navigationTitle("Stored provider history").task { load(0) }.toolbar {
      Button("Refresh", systemImage: "arrow.clockwise") {
        messages = []
        load(0)
      }.disabled(busy)
    }
  }
  private func load(_ cursor: Int) {
    busy = true
    model.perform {
      defer { busy = false }
      guard let client = model.client else { return }
      let page: TranscriptPage = try await client.request(
        "v1/sessions/\(sessionID)/history", query: ["offset": String(cursor)])
      if cursor == 0 { messages = page.messages } else { messages += page.messages }
      next = page.next_offset
      offset = cursor
      status =
        page.truncated
        ? "Some source records exceed the history record limit."
        : messages.isEmpty ? "No stored transcript messages." : ""
    }
  }
}
