import Foundation
import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
#if os(macOS)
  import AppKit
#else
  import UIKit
  import SafariServices
#endif

/// Keeps the native app active for OpenAI's required HTTP loopback callback.
@MainActor final class ChatGPTBrowserSignIn: NSObject, ObservableObject {
  @Published private(set) var isSigningIn = false
  private var operation: Task<ChatGPTAccount, Error>?
  private var listener: ChatGPTLoopback?
  private var attemptID: UUID?
  var isPresentingBrowser: Bool {
    #if os(iOS)
      safari != nil
    #else
      false
    #endif
  }
  #if os(iOS)
    private var safari: SFSafariViewController?
    private var backgroundObserver: NSObjectProtocol?
  #endif

  func signIn(accountID: String? = nil) async throws -> ChatGPTAccount {
    guard operation == nil else {
      throw OrchestratorError.invalid("A ChatGPT sign-in is already open.")
    }
    isSigningIn = true
    let callback = ChatGPTLoopback()
    listener = callback
    let task = Task { @MainActor in
      let redirect = try await callback.start()
      let request = try await ChatGPTAuth.shared.prepareSignIn(
        redirectURI: redirect, accountID: accountID)
      self.attemptID = request.id
      guard
        let state = URLComponents(url: request.authorizationURL, resolvingAgainstBaseURL: false)?
          .queryItems?.first(where: { $0.name == "state" })?.value
      else {
        throw OrchestratorError.invalid("Could not prepare ChatGPT sign-in.")
      }
      await callback.setExpectedState(state)
      try Task.checkCancellation()
      try self.openBrowser(request.authorizationURL)
      let result = try await callback.waitForCallback()
      try Task.checkCancellation()
      return try await ChatGPTAuth.shared.completeSignIn(attemptID: request.id, callbackURL: result)
    }
    operation = task
    defer {
      callback.stop()
      closeBrowser()
      operation = nil
      listener = nil
      isSigningIn = false
      if let id = attemptID { Task { await ChatGPTAuth.shared.cancelSignIn(attemptID: id) } }
      attemptID = nil
    }
    return try await withTaskCancellationHandler {
      try await task.value
    } onCancel: {
      Task { @MainActor [weak self] in self?.cancel() }
    }
  }

  func cancel() {
    operation?.cancel()
    listener?.stop()
    closeBrowser()
    if let id = attemptID { Task { await ChatGPTAuth.shared.cancelSignIn(attemptID: id) } }
  }

  private func openBrowser(_ url: URL) throws {
    #if os(macOS)
      guard NSWorkspace.shared.open(url) else {
        throw OrchestratorError.invalid("Could not open the browser for ChatGPT sign-in.")
      }
    #else
      guard
        let scene = UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }).first(
          where: { $0.activationState == .foregroundActive }),
        var presenter = scene.windows.first(where: \.isKeyWindow)?.rootViewController
      else {
        throw OrchestratorError.invalid(
          "Open Control Plane in the foreground to sign in with ChatGPT.")
      }
      while let shown = presenter.presentedViewController { presenter = shown }
      let controller = SFSafariViewController(url: url)
      controller.delegate = self
      safari = controller
      presenter.present(controller, animated: true)
      backgroundObserver = NotificationCenter.default.addObserver(
        forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main
      ) { [weak self] _ in
        Task { @MainActor in self?.cancel() }
      }
    #endif
  }

  private func closeBrowser() {
    #if os(iOS)
      if let observer = backgroundObserver {
        NotificationCenter.default.removeObserver(observer)
        backgroundObserver = nil
      }
      safari?.delegate = nil
      safari?.dismiss(animated: true)
      safari = nil
    #endif
  }
}

#if os(iOS)
  extension ChatGPTBrowserSignIn: SFSafariViewControllerDelegate {
    nonisolated func safariViewControllerDidFinish(_ controller: SFSafariViewController) {
      Task { @MainActor [weak self] in self?.cancel() }
    }
  }
#endif
