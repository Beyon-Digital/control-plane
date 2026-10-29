import SwiftUI
import UserNotifications

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
#if os(macOS)
  import AppKit
#else
  import UIKit
#endif

extension Notification.Name {
  static let controlPlaneNotificationTapped = Notification.Name(
    "dev.controlplane.notification-tapped")
}
@MainActor final class PushCoordinator: ObservableObject {
  static let shared = PushCoordinator()
  @Published private(set) var status = "Notifications are off until you enable them for a host."
  @Published private(set) var enabledHosts: Set<String>
  @Published private(set) var busy = false
  private var pendingRemoval: Set<String>
  private var active: (APIClient, String)?
  private let tokenKey = "control-plane-apns-device-token"
  private init() {
    enabledHosts = Set(UserDefaults.standard.stringArray(forKey: "push-enabled-hosts") ?? [])
    pendingRemoval = Set(UserDefaults.standard.stringArray(forKey: "push-pending-removal") ?? [])
  }
  private func persist() {
    UserDefaults.standard.set(Array(enabledHosts).sorted(), forKey: "push-enabled-hosts")
    UserDefaults.standard.set(Array(pendingRemoval).sorted(), forKey: "push-pending-removal")
  }
  private var bundled: Bool { Bundle.main.bundleIdentifier != nil }
  private var environment: String {
    if let value = Bundle.main.object(forInfoDictionaryKey: "ControlPlaneAPNsEnvironment")
      as? String, ["sandbox", "production"].contains(value)
    {
      return value
    }
    #if DEBUG
      return "sandbox"
    #else
      return "production"
    #endif
  }
  func enable(client: APIClient, host: String) async {
    guard !busy else { return }
    guard bundled else {
      status = "Notifications require the signed app bundle."
      return
    }
    busy = true
    defer { busy = false }
    do {
      let allowed = try await UNUserNotificationCenter.current().requestAuthorization(options: [
        .alert, .badge, .sound,
      ])
      guard allowed else {
        status = "Notifications were not allowed. You can change this in system Settings."
        return
      }
      enabledHosts.insert(host)
      pendingRemoval.remove(host)
      persist()
      active = (client, host)
      status = "Registering this device with APNs…"
      registerWithAPNs()
      if let token = CredentialStore.load(host: tokenKey) {
        try await send(token, client: client)
        status = "Notifications enabled for this host."
      }
    } catch { status = error.localizedDescription }
  }
  func sync(client: APIClient, host: String) async {
    active = (client, host)
    if pendingRemoval.contains(host) {
      do {
        try await client.mutate("v1/notifications/unregister")
        pendingRemoval.remove(host)
        persist()
      } catch {
        status = "Remote notification removal is still pending."
        return
      }
    }
    guard enabledHosts.contains(host), bundled else { return }
    let settings = await UNUserNotificationCenter.current().notificationSettings()
    guard
      settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional
    else {
      status = "Enable notification permission in system Settings to receive alerts."
      return
    }
    registerWithAPNs()
    if let token = CredentialStore.load(host: tokenKey) {
      do {
        try await send(token, client: client)
        status = "Notifications enabled for this host."
      } catch { status = error.localizedDescription }
    }
  }
  func disable(client: APIClient, host: String) async {
    enabledHosts.remove(host)
    pendingRemoval.insert(host)
    persist()
    do {
      try await client.mutate("v1/notifications/unregister")
      pendingRemoval.remove(host)
      persist()
      status = "Notifications disabled for this host."
    } catch {
      status = "Disabled locally. Remote removal will be retried when this host reconnects."
    }
  }
  func registered(_ data: Data) {
    let token = data.map { String(format: "%02x", $0) }.joined()
    do { try CredentialStore.save(token, host: tokenKey) } catch {
      status = error.localizedDescription
      return
    }
    guard let (client, host) = active, enabledHosts.contains(host) else { return }
    Task {
      do {
        try await send(token, client: client)
        status = "Notifications enabled for this host."
      } catch { status = error.localizedDescription }
    }
  }
  func failed(_ error: Error) {
    status =
      "APNs registration failed: \(error.localizedDescription). Check the signed app's push entitlement and provisioning profile."
  }
  private func send(_ token: String, client: APIClient) async throws {
    try await client.mutate(
      "v1/notifications/register",
      body: ["apns_token": .string(token), "environment": .string(environment)])
  }
  private func registerWithAPNs() {
    #if os(macOS)
      NSApplication.shared.registerForRemoteNotifications()
    #else
      UIApplication.shared.registerForRemoteNotifications()
    #endif
  }
}
final class PushApplicationDelegate: NSObject, UNUserNotificationCenterDelegate {
  func userNotificationCenter(
    _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) { completionHandler([.banner, .sound]) }
  func userNotificationCenter(
    _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    let payload = response.notification.request.content.userInfo
    Task { @MainActor in
      NotificationCenter.default.post(
        name: .controlPlaneNotificationTapped, object: nil, userInfo: payload)
      completionHandler()
    }
  }
}
#if os(macOS)
  extension PushApplicationDelegate: NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
      if Bundle.main.bundleIdentifier != nil { UNUserNotificationCenter.current().delegate = self }
    }
    func application(
      _ application: NSApplication,
      didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) { Task { @MainActor in PushCoordinator.shared.registered(deviceToken) } }
    func application(
      _ application: NSApplication, didFailToRegisterForRemoteNotificationsWithError error: Error
    ) { Task { @MainActor in PushCoordinator.shared.failed(error) } }
  }
#else
  extension PushApplicationDelegate: UIApplicationDelegate {
    func application(
      _ application: UIApplication,
      didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
      UNUserNotificationCenter.current().delegate = self
      return true
    }
    func application(
      _ application: UIApplication,
      didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) { Task { @MainActor in PushCoordinator.shared.registered(deviceToken) } }
    func application(
      _ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error
    ) { Task { @MainActor in PushCoordinator.shared.failed(error) } }
  }
#endif
struct PushSettingsView: View {
  @EnvironmentObject private var model: AppModel
  @StateObject private var push = PushCoordinator.shared
  var body: some View {
    Section("Host alerts") {
      Text(push.status).font(.caption).foregroundStyle(.secondary)
      if let client = model.client {
        if push.enabledHosts.contains(model.currentAddress) {
          Button("Disable alerts for this host") {
            Task { await push.disable(client: client, host: model.currentAddress) }
          }
        } else {
          Button("Enable approval and completion alerts") {
            Task { await push.enable(client: client, host: model.currentAddress) }
          }
        }
      } else {
        Text("Connect to a host before enabling alerts.").foregroundStyle(.secondary)
      }
      Text(
        "The host needs a configured APNs gateway. Notification payloads contain opaque event identifiers; session content stays on your host."
      ).font(.caption2).foregroundStyle(.secondary)
    }.disabled(push.busy)
  }
}
