import SwiftUI
#if os(iOS)
import UIKit
#endif
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@MainActor struct ConnectView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @StateObject private var discovery = HostDiscovery()
    @State private var address = ""
    @State private var token = ""
    @State private var secret = ""
    #if os(iOS)
    @State private var deviceName = "My iPhone"
    #else
    @State private var deviceName = "My Mac"
    #endif
    @State private var method = "pair"
    @State private var enrollmentKind = "device"
    @State private var grpcAddress = ""
    @State private var caPEM = ""
    @State private var caFingerprint = ""
    @State private var trustMessage = ""
    @State private var showOptions = false
    @State private var busy = false
    @State private var error: String?
    @State private var showSettingsHelp = false
    @State private var connectionTask: Task<Void, Never>?
    private var normalizedAddress: String {
        let value = address.trimmingCharacters(in: .whitespacesAndNewlines)
        return value.isEmpty || value.contains("://") ? value : "https://" + value
    }
    private var canConnect: Bool {
        !busy && !normalizedAddress.isEmpty && !(method == "pair" ? secret : token).trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("https://your-mac.local:9847", text: $address)
                        .autocorrectionDisabled().accessibilityIdentifier("connection.address")
                        #if os(iOS)
                        .keyboardType(.URL).textInputAutocapitalization(.never)
                        #endif
                } header: { Text("Machine address") } footer: {
                    Text("Use the private address supplied by your host. Shared machines include an account path.")
                }
                Section {
                    Picker("Connect using", selection: $method) {
                        Text("Pairing code").tag("pair")
                        Text("Device token").tag("token")
                    }.pickerStyle(.segmented).accessibilityIdentifier("connection.method")
                    if method == "pair" {
                        SecureField("Single-use pairing code", text: $secret).accessibilityIdentifier("connection.secret")
                            #if os(iOS)
                            .textContentType(.oneTimeCode)
                            #endif
                        TextField("Device name", text: $deviceName)
                        Picker("Enrollment", selection: $enrollmentKind) {
                            Text("My device").tag("device")
                            Text("Team invitation").tag("invitation")
                        }
                    } else {
                        SecureField("Device token", text: $token).accessibilityIdentifier("connection.token")
                            #if os(iOS)
                            .textContentType(.oneTimeCode)
                            #endif
                    }
                } header: { Text("Authorization") } footer: {
                    Text(method == "pair" ? "Get a pairing code from the machine's administrator. Each code works once." : "Use a token already issued to this device. Credentials are stored in Keychain.")
                }
                Section("Nearby machines") {
                    Button(discovery.searching ? "Stop searching" : "Find nearby machines", systemImage: "network") {
                        if discovery.searching { discovery.stop() } else { discovery.start() }
                    }
                    if discovery.searching || !discovery.hosts.isEmpty { Text(discovery.status).font(.caption).foregroundStyle(.secondary) }
                    ForEach(discovery.hosts) { host in
                        Button {
                            address = host.address; token = ""; secret = ""
                        } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(host.name)
                                Text(host.address).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
                Section {
                    DisclosureGroup("Connection options", isExpanded: $showOptions) {
                        TextField("Optional gRPC URL", text: $grpcAddress).autocorrectionDisabled()
                            #if os(iOS)
                            .keyboardType(.URL).textInputAutocapitalization(.never)
                            #endif
                        Text("gRPC uses the same machine and its separate gRPC port.").font(.caption).foregroundStyle(.secondary)
                        HostCATrustInputs(address: normalizedAddress, pem: $caPEM, fingerprint: $caFingerprint, message: $trustMessage)
                        if !trustMessage.isEmpty { Text(trustMessage).font(.caption) }
                    }
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Connect machine")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            .scrollDismissesKeyboard(.interactively)
            #endif
            .toolbar { ToolbarItem(placement: .cancellationAction) {
                Button("Cancel") { connectionTask?.cancel(); dismiss() }.accessibilityIdentifier("connection.cancel")
            } }
            .safeAreaInset(edge: .bottom) {
                VStack(alignment: .leading, spacing: 12) {
                if let error { Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true) }
                #if os(iOS)
                if showSettingsHelp { Button("Open app settings") { if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) } } }
                #endif
                Button { connect() } label: {
                    HStack {
                        if busy { ProgressView().tint(.white) }
                        Text(busy ? "Connecting…" : method == "pair" ? "Pair and connect" : "Connect")
                            .fontWeight(.semibold)
                    }.frame(maxWidth: .infinity, minHeight: 28)
                }.buttonStyle(.borderedProminent).controlSize(.large)
                    .disabled(!canConnect).accessibilityIdentifier("connection.submit")
                }.padding().background(.bar)
            }
        }
        #if os(macOS)
        .frame(minWidth: 400, idealWidth: 480, minHeight: 560)
        #endif
        .onDisappear { discovery.stop(); connectionTask?.cancel() }
    }
    private func connect() {
        #if os(iOS)
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        #endif
        busy = true; error = nil; showSettingsHelp = false
        connectionTask = Task {
            defer { busy = false }
            do {
                let bootstrap = try APIClient(address: normalizedAddress, token: method == "pair" ? "" : token, grpcAddress: "")
                if method == "pair" {
                    let paired = try await DeviceIdentity.pair(using: bootstrap, secret: secret, name: deviceName, invitation: enrollmentKind == "invitation")
                    token = paired.token; secret = ""; method = "token"
                    try CredentialStore.save(token, host: bootstrap.baseURL.absoluteString)
                } else if bootstrap.baseURL.scheme == "https", DeviceIdentity.certificate(host: bootstrap.baseURL.absoluteString) == nil {
                    _ = try await DeviceIdentity.enroll(using: bootstrap)
                }
                UserDefaults.standard.set(grpcAddress, forKey: "grpc:" + bootstrap.baseURL.absoluteString)
                try await model.connect(address: normalizedAddress, token: token)
                dismiss()
            } catch is CancellationError {} catch {
                if let issue = error as? URLError, [.serverCertificateUntrusted, .serverCertificateHasUnknownRoot].contains(issue.code) {
                    showOptions = true
                    self.error = "Verify this machine's private certificate in Connection options before pairing."
                } else if let issue = error as? URLError, issue.code == .notConnectedToInternet {
                    showSettingsHelp = true
                    self.error = "Can't reach this machine. Check your private network and Local Network access in Settings, then try again."
                } else { self.error = error.localizedDescription }
            }
        }
    }
}
