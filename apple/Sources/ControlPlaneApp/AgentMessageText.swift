import SwiftUI
#if os(iOS)
import UIKit
#else
import AppKit
#endif

struct AgentMessageText: View {
    let text: String
    var body: some View {
        let parts = text.components(separatedBy: "```")
        VStack(alignment: .leading, spacing: 12) {
            ForEach(parts.indices, id: \.self) { index in
                if index % 2 == 0 {
                    if !parts[index].isEmpty { Text(.init(parts[index])).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                } else { AgentCodeBlock(content: parts[index]) }
            }
        }
    }
}

private struct AgentCodeBlock: View {
    let content: String
    @State private var copied = false
    private var pieces: (String, String) {
        let lines = content.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false)
        let language = String(lines[0]).trimmingCharacters(in: .whitespaces)
        if lines.count == 2, language.isEmpty || language.range(of: "^[A-Za-z0-9_+.#-]{1,24}$", options: .regularExpression) != nil {
            return (language.isEmpty ? "Code" : language, String(lines[1]).trimmingCharacters(in: .newlines))
        }
        return ("Code", content.trimmingCharacters(in: .newlines))
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(pieces.0).font(.caption.weight(.medium)).foregroundStyle(.secondary)
                Spacer()
                Button(copied ? "Copied" : "Copy") {
                    #if os(iOS)
                    UIPasteboard.general.string = pieces.1
                    copied = true
                    #else
                    NSPasteboard.general.clearContents()
                    copied = NSPasteboard.general.setString(pieces.1, forType: .string)
                    #endif
                }.font(.caption.weight(.medium)).buttonStyle(.plain).frame(minWidth: 44, minHeight: 44).accessibilityLabel(copied ? "Code copied" : "Copy code")
            }.padding(.horizontal, 12)
            Divider()
            ScrollView(.horizontal) {
                Text(pieces.1).font(.system(.subheadline, design: .monospaced))
                    .lineLimit(nil).fixedSize(horizontal: true, vertical: true)
                    .textSelection(.enabled).padding(12)
            }
        }.background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
            .onChange(of: content) { _, _ in copied = false }
    }
}
