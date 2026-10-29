import Foundation
import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif

struct OrchestratorChatEntry: Identifiable {
  let id = UUID()
  var role: String
  var text: String
  var note: String?
}

@MainActor final class OrchestratorModel: ObservableObject {
  @Published var draft = ""
  @Published var allowManagement = false
  @Published private(set) var entries: [OrchestratorChatEntry] = []
  @Published private(set) var tools: [String] = []
  @Published private(set) var running = false
  @Published private(set) var status = ""
  @Published private(set) var error: String?
  private var context: [InferenceMessage] = []
  private var task: Task<Void, Never>?
  private var generation = UUID()
  private var assistantID: UUID?
  private var activeManagement = false
  private var boundHost = ""
  private var boundProfile: ModelProfile?

  func bind(host: String, profile: ModelProfile?) {
    guard boundHost != host || boundProfile != profile else { return }
    reset()
    boundHost = host
    boundProfile = profile
    allowManagement = false
  }
  func reset() {
    let hadEffects = (running && activeManagement) || status.hasPrefix("Previous turn cancelled.")
    cancel()
    context = []
    entries = []
    tools = []
    error = nil
    status =
      hadEffects
      ? "Previous turn cancelled. Some agent actions may have completed; check the previous host’s sessions."
      : "New conversation"
  }
  func credentialsWillChange() {
    reset()
    allowManagement = false
  }
  func cancel() {
    guard running else { return }
    task?.cancel()
    task = nil
    generation = UUID()
    running = false
    finishPartial("Cancelled. This turn was not added to context." + warning(activeManagement))
    status = "Cancelled"
  }
  func send(profile: ModelProfile, client: APIClient?, host: String) {
    guard !running else { return }
    bind(host: host, profile: profile)
    let prompt = draft.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !prompt.isEmpty else { return }
    guard prompt.utf8.count <= 32768 else {
      error = "Keep each prompt under 32 KiB."
      return
    }
    do { _ = try profile.validatedURL() } catch {
      self.error = error.localizedDescription
      return
    }
    guard context.count < 120 else {
      error = "Start a new conversation to reset context."
      return
    }
    let pending = context + [InferenceMessage(role: "user", content: prompt)]
    let management = allowManagement && client != nil
    let turn = UUID()
    generation = turn
    activeManagement = management
    entries.append(OrchestratorChatEntry(role: "user", text: prompt))
    let response = OrchestratorChatEntry(role: "assistant", text: "")
    assistantID = response.id
    entries.append(response)
    tools = []
    draft = ""
    error = nil
    running = true
    status = "Thinking…"
    task = Task { [weak self] in
      guard let self else { return }
      do {
        let result = try await OrchestratorEngine(client: client, allowManagement: management).run(
          profile: profile, messages: pending,
          onDelta: { [weak self] delta in Task { @MainActor in self?.append(delta, turn: turn) } },
          onTool: { [weak self] tool in Task { @MainActor in self?.record(tool, turn: turn) } })
        try Task.checkCancellation()
        guard generation == turn else { return }
        context = result
        if let id = assistantID, let index = entries.firstIndex(where: { $0.id == id }) {
          entries[index].text =
            result.last(where: { $0.role == "assistant" && !$0.content.isEmpty })?.content
            ?? "Completed."
        }
        assistantID = nil
        running = false
        task = nil
        status = "Completed"
      } catch {
        guard generation == turn else { return }
        running = false
        task = nil
        let cancelled = error is CancellationError || Task.isCancelled
        self.error = cancelled ? nil : error.localizedDescription
        finishPartial(
          (cancelled ? "Cancelled" : error.localizedDescription)
            + ". This turn was not added to context." + warning(management))
        status = cancelled ? "Cancelled" : "Turn failed"
      }
    }
  }
  private func append(_ delta: String, turn: UUID) {
    guard running, generation == turn, let id = assistantID,
      let index = entries.firstIndex(where: { $0.id == id })
    else { return }
    if entries[index].text.utf8.count + delta.utf8.count > 262144 {
      error = "Response exceeded the display limit."
      cancel()
      return
    }
    entries[index].text += delta
  }
  private func record(_ value: String, turn: UUID) {
    guard running, generation == turn else { return }
    if tools.count < 100 { tools.append(String(value.prefix(4096))) }
    status = "Using host tools…"
  }
  private func finishPartial(_ note: String) {
    if let id = assistantID, let index = entries.firstIndex(where: { $0.id == id }) {
      if entries[index].text.isEmpty { entries[index].text = "Turn stopped" }
      entries[index].note = note
    }
    assistantID = nil
  }
  private func warning(_ enabled: Bool) -> String {
    enabled ? " Some agent actions may have completed; check sessions before retrying." : ""
  }
}
