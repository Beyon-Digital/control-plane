import SwiftUI

#if canImport(ControlPlaneKit)
  import ControlPlaneKit
#endif
private struct NativeMember: Decodable, Identifiable {
  let id: String
  let label: String
  let role: String
  let revoked: Bool
}
private struct NativeIdentity: Decodable {
  let member_id: String
  let role: String
}
private struct SessionGrant: Decodable, Identifiable {
  var id: String { member_id }
  let member_id: String
  let role: String
}
private struct SessionPolicy: Decodable {
  let owner: String
  let shared: Bool
  let can_manage: Bool?
  let grants: [SessionGrant]
}
struct SessionAccessView: View {
  @EnvironmentObject private var model: AppModel
  let sessionID: String
  @State private var policy: SessionPolicy?
  @State private var members: [NativeMember] = []
  @State private var memberID = ""
  @State private var role = "viewer"
  @State private var loading = false
  var body: some View {
    Form {
      if let policy {
        Section("Owner") {
          Text(members.first { $0.id == policy.owner }?.label ?? policy.owner).textSelection(
            .enabled)
          Text(
            "An operator can execute code with this OS account’s authority. Use separate account workers for untrusted members."
          ).font(.caption).foregroundStyle(.secondary)
        }
        Section("Session grants") {
          ForEach(policy.grants) { grant in
            HStack {
              VStack(alignment: .leading) {
                Text(members.first { $0.id == grant.member_id }?.label ?? grant.member_id)
                Text(grant.role.capitalized).font(.caption)
              }
              Spacer()
              Button("Remove", role: .destructive) { save(grant.member_id, revoke: true) }
            }.disabled(policy.can_manage != true || loading)
          }
          if policy.grants.isEmpty { Text("No explicit grants.") }
        }
        Section("Grant access") {
          Picker("Member", selection: $memberID) {
            Text("Select a team member").tag("")
            ForEach(members.filter { !$0.revoked && $0.id != policy.owner }) {
              Text($0.label).tag($0.id)
            }
          }
          Picker("Permission", selection: $role) {
            Text("View session").tag("viewer")
            Text("Interact with session").tag("operator")
          }
          Button("Save permission") { save(memberID, revoke: false) }.disabled(
            memberID.isEmpty || loading)
        }.disabled(policy.can_manage != true)
      } else {
        ProgressView("Loading access…")
      }
    }.navigationTitle("Session permissions").task { load() }
  }
  private func load() {
    loading = true
    model.perform {
      defer { loading = false }
      guard let client = model.client else { return }
      policy = try await client.request("v1/sessions/\(sessionID)/access")
      members = try await client.request("v1/members")
    }
  }
  private func save(_ member: String, revoke: Bool) {
    loading = true
    model.perform {
      defer { loading = false }
      guard let client = model.client else { return }
      var body: [String: JSONValue] = ["member_id": .string(member)]
      if revoke { body["revoke"] = .bool(true) } else { body["role"] = .string(role) }
      try await client.mutate("v1/sessions/\(sessionID)/access", body: body)
      policy = try await client.request("v1/sessions/\(sessionID)/access")
    }
  }
}
struct TeamMembersView: View {
  @EnvironmentObject private var model: AppModel
  @State private var members: [NativeMember] = []
  @State private var identity: NativeIdentity?
  @State private var invitation = ""
  @State private var label = ""
  @State private var role = "operator"
  @State private var expires = ""
  @State private var deviceSecret = ""
  @State private var pendingInvitations: [JSONValue] = []
  @State private var lifetime = 86400
  @State private var revoking: NativeMember?
  @State private var loading = false
  var body: some View {
    Form {
      Section("Team members") {
        ForEach(members) { member in
          HStack {
            VStack(alignment: .leading) {
              Text(member.label)
              Text(member.revoked ? "Revoked" : member.role.capitalized).font(.caption)
              Text(member.id).font(.caption2.monospaced()).textSelection(.enabled)
            }
            Spacer()
            if !member.revoked
              && (identity?.role == "administrator" || identity?.member_id == member.id)
            {
              Menu("Add device") {
                ForEach(deviceRoles(member), id: \.self) { role in
                  Button(role.capitalized) {
                    model.perform {
                      guard let client = model.client else { return }
                      let result: JSONValue = try await client.request(
                        "v1/members/\(member.id)/enrollment", method: "POST",
                        body: ["role": .string(role)])
                      deviceSecret = result["secret"]?.text ?? ""
                    }
                  }
                }
              }
            }
            if identity?.role == "administrator" && !member.revoked
              && member.id != identity?.member_id
            {
              Button("Revoke", role: .destructive) { revoking = member }
            }
          }
        }
      }
      if identity?.role == "administrator" {
        Section("Invite a member") {
          TextField("Member label", text: $label)
          Picker("Role", selection: $role) {
            Text("Operator").tag("operator")
            Text("Viewer").tag("viewer")
            Text("Administrator").tag("administrator")
          }
          Picker("Expires after", selection: $lifetime) {
            Text("One hour").tag(3600)
            Text("One day").tag(86400)
            Text("One week").tag(604800)
          }
          Button("Create invitation") {
            loading = true
            model.perform {
              defer { loading = false }
              guard let client = model.client else { return }
              let result: JSONValue = try await client.request(
                "v1/invitations", method: "POST",
                body: [
                  "role": .string(role), "label": .string(label),
                  "expires_in_seconds": .number(Double(lifetime)),
                ])
              invitation = result["secret"]?.text ?? ""
              expires = result["expires_at"]?.text ?? ""
            }
          }.disabled(label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || loading)
          if !invitation.isEmpty {
            Text(invitation).font(.body.monospaced()).textSelection(.enabled).privacySensitive()
            Text(
              "Single-use invitation. Share it directly with the intended member. Expires: \(expires)"
            ).font(.caption)
          }
        }
      }
      if !deviceSecret.isEmpty {
        Section("Single-use device enrollment") {
          Text(deviceSecret).font(.body.monospaced()).textSelection(.enabled).privacySensitive()
          Text("Choose Pair my device on the other device. This secret expires in five minutes.")
            .font(.caption)
        }
      }
      if identity?.role == "administrator" {
        Section("Pending invitations") {
          ForEach(Array(pendingInvitations.enumerated()), id: \.offset) { _, invitation in
            HStack {
              Text(invitation["label"]?.text ?? "Member invitation")
              Spacer()
              Button("Revoke", role: .destructive) {
                model.perform {
                  guard let client = model.client, let id = invitation["member_id"]?.text else {
                    return
                  }
                  try await client.mutate("v1/invitations/\(id)/revoke")
                  load()
                }
              }
            }
          }
        }
      }
      if loading { ProgressView() }
    }.navigationTitle("Team members").task { load() }.toolbar {
      Button("Refresh", systemImage: "arrow.clockwise") { load() }.disabled(loading)
    }.confirmationDialog(
      "Revoke \(revoking?.label ?? "this member") and stop their active work?",
      isPresented: Binding(get: { revoking != nil }, set: { if !$0 { revoking = nil } }),
      titleVisibility: .visible
    ) {
      Button("Revoke member", role: .destructive) {
        guard let member = revoking else { return }
        model.perform {
          guard let client = model.client else { return }
          try await client.mutate("v1/members/\(member.id)/revoke")
          revoking = nil
          load()
        }
      }
    }
  }
  private func load() {
    loading = true
    model.perform {
      defer { loading = false }
      guard let client = model.client else { return }
      identity = try await client.request("v1/me")
      members = try await client.request("v1/members")
      if identity?.role == "administrator" {
        let all: [JSONValue] = try await client.request("v1/invitations")
        pendingInvitations = all.filter {
          if case .null = $0["consumed_at"], case .number(let expires) = $0["expires_at"] {
            return expires > Date().timeIntervalSince1970
          }
          return false
        }
      }
    }
  }
  private func deviceRoles(_ member: NativeMember) -> [String] {
    let roles = ["viewer", "operator", "administrator"]
    let cap = min(
      roles.firstIndex(of: identity?.role ?? "viewer") ?? 0, roles.firstIndex(of: member.role) ?? 0)
    return Array(roles.prefix(cap + 1))
  }
}
struct ResourcePolicyView: View {
  @EnvironmentObject private var model: AppModel
  @State private var policy: [String: JSONValue] = [:]
  @State private var usage: JSONValue = .null
  @State private var values: [String: String] = [:]
  @State private var editable = false
  @State private var busy = false
  @State private var saved = ""
  var body: some View {
    Form {
      Section("Host usage") {
        Text(usage.prettyPrinted).font(.caption.monospaced()).textSelection(.enabled)
      }
      Section("Resource and retention policy") {
        ForEach(policy.keys.sorted(), id: \.self) { key in
          TextField(
            key.replacingOccurrences(of: "_", with: " ").capitalized,
            text: Binding(get: { values[key] ?? "" }, set: { values[key] = $0 }))
        }
      }.disabled(!editable || busy)
      if editable {
        Button("Save host policy") {
          busy = true
          model.perform {
            defer { busy = false }
            guard let client = model.client else { return }
            var replacement = policy
            for (key, value) in policy {
              if case .number = value {
                guard let text = values[key], let number = Double(text), number.isFinite,
                  number >= 0, number.rounded() == number
                else { throw APIError.credentials("\(key) must be a nonnegative integer.") }
                replacement[key] = .number(number)
              }
            }
            try await client.mutate("v1/resources", body: ["policy": .object(replacement)])
            saved = "Host policy saved"
            load()
          }
        }.disabled(busy)
      } else {
        Text("Only a host administrator can change these settings.").font(.caption)
      }
      if busy { ProgressView() }
      if !saved.isEmpty { Text(saved).font(.caption) }
    }.navigationTitle("Resources and retention").task { load() }
  }
  private func load() {
    busy = true
    model.perform {
      defer { busy = false }
      guard let client = model.client else { return }
      let result: JSONValue = try await client.request("v1/resources")
      if case .object(let data) = result["policy"] {
        policy = data
        values = data.mapValues { $0.text }
      }
      usage = result["usage"] ?? .null
      if case .bool(let value) = result["can_manage"] { editable = value }
    }
  }
}
