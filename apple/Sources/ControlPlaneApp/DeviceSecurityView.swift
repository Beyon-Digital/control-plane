import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
struct DeviceSecurityView: View {
  @EnvironmentObject private var model: AppModel
  @State private var certificate: DeviceCertificateInfo?
  @State private var pem = ""
  @State private var fingerprint = ""
  @State private var message = ""
  @State private var busy = false
  var body: some View {
    Form {
      Section("This device") {
        if let certificate {
          LabeledContent("Device", value: certificate.device_id)
          LabeledContent(
            "Expires",
            value: Date(timeIntervalSince1970: Double(certificate.expires_at)).formatted())
          Text(certificate.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
        } else {
          Text("This connection has no enrolled device certificate.")
        }
        Button(certificate == nil ? "Enroll this device certificate" : "Renew certificate") {
          busy = true
          model.perform {
            defer { busy = false }
            guard let token = CredentialStore.load(host: model.currentAddress) else {
              throw APIError.credentials("Pair this device first.")
            }
            let client = try APIClient(address: model.currentAddress, token: token, grpcAddress: "")
            certificate = try await DeviceIdentity.enroll(using: client)
            try await model.connect(address: model.currentAddress, token: token)
            message = "The device’s protected Keychain key is now bound to this credential."
          }
        }.disabled(busy || model.client == nil || !model.currentAddress.hasPrefix("https://"))
      }
      HostCATrustFields(
        address: model.currentAddress, pem: $pem, fingerprint: $fingerprint, message: $message)
      if busy { ProgressView() }
      if !message.isEmpty { Text(message).font(.caption) }
    }.navigationTitle("Device security").task {
      certificate = DeviceIdentity.certificate(host: model.currentAddress)
    }
  }
}
struct HostCATrustFields: View {
  let address: String
  @Binding var pem: String
  @Binding var fingerprint: String
  @Binding var message: String
  var body: some View {
    Section("Private host certificate authority") {
      HostCATrustInputs(address: address, pem: $pem, fingerprint: $fingerprint, message: $message)
    }
  }
}
struct HostCATrustInputs: View {
  let address: String
  @Binding var pem: String
  @Binding var fingerprint: String
  @Binding var message: String
  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      Text(
        "Obtain the CA PEM and SHA-256 fingerprint directly from your host administrator. This trust applies only to this app’s selected host address."
      ).font(.caption).foregroundStyle(.secondary)
      Text("CA certificate (PEM)").font(.subheadline)
      TextEditor(text: $pem).font(.caption.monospaced()).frame(minHeight: 80).accessibilityLabel("CA certificate PEM")
      TextField("Verified CA SHA-256 fingerprint", text: $fingerprint).autocorrectionDisabled()
      Button("Trust this host CA") {
        do {
          let client = try APIClient(address: address, token: "", grpcAddress: "")
          guard client.baseURL.scheme == "https" else { throw APIError.insecureAddress }
          try DeviceIdentity.trustHostCA(
            pem, host: client.baseURL.absoluteString, expectedSHA256: fingerprint)
          message = "Host CA saved. Reconnect to use it."
        } catch { message = error.localizedDescription }
      }.disabled(pem.isEmpty || fingerprint.isEmpty)
    }
  }
}
