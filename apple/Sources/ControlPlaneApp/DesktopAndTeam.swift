import SwiftUI
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

private struct DesktopDisplayChoice:Identifiable{let id:String;let name:String}
struct DesktopView: View {
    @EnvironmentObject private var model: AppModel
    @State private var frame: DesktopFrame?
    @State private var status = "Checking desktop availability…"
    @State private var lease: String?
    @State private var text = ""
    @State private var displays:[DesktopDisplayChoice]=[]
    @State private var selectedDisplayID=""
    @State private var autoRefresh = false
    @State private var busy = false
    @StateObject private var video=VideoStreamModel()
    var body: some View {
        VStack {
            if displays.count>1{Picker("Display",selection:Binding(get:{selectedDisplayID},set:{selectDisplay($0)})){ForEach(displays){Text($0.name).tag($0.id)}}.padding(.horizontal).disabled(busy)}
            HStack {
                Text(status).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button(video.running ? "Stop video" : "Live video",systemImage:video.running ? "stop.fill":"video") {
                    if video.running {video.stop()} else if let client=model.client {video.start(client)}
                }
                Button("Capture", systemImage: "camera") { capture() }.disabled(busy)
                Button(lease == nil ? "Take control" : "Renew control") { takeControl() }
            }.padding()
            GeometryReader { geometry in
                ZStack {
                    Color.black.opacity(0.9)
                    if let image=displayImage {
                        let dimensions=displayDimensions
                        let scale = min(geometry.size.width / dimensions.width, geometry.size.height / dimensions.height)
                        image.resizable().interpolation(.high).scaledToFit()
                            .frame(width: dimensions.width * scale, height: dimensions.height * scale)
                            .contentShape(Rectangle())
                            .gesture(SpatialTapGesture().onEnded { tap in
                                guard lease != nil else { return }
                                let input=inputDimensions
                                action(["type": .string("click"), "x": .number(tap.location.x / scale * input.width/dimensions.width), "y": .number(tap.location.y / scale * input.height/dimensions.height)])
                            })
                    } else { ContentUnavailableView("Desktop snapshots", systemImage: "display", description: Text("Capture a frame from the logged-in graphical session. Screen and input permissions must be granted on the host.")).foregroundStyle(.white) }
                }
            }
            HStack {
                TextField("Text to type on remote machine", text: $text).textFieldStyle(.roundedBorder)
                Button("Type") { action(["type": .string("text"), "text": .string(text)]); text = "" }.disabled(lease == nil || text.isEmpty)
                Menu("Keys") {
                    ForEach(["enter", "escape", "tab", "backspace", "up", "down", "left", "right"], id: \.self) { key in Button(key.capitalized) { action(["type": .string("key"), "key": .string(key)]) } }
                }.disabled(lease == nil)
                Button("Scroll ↑") { action(["type": .string("scroll"), "dy": .number(-3), "dx": .number(0)]) }.disabled(lease == nil)
                Button("Scroll ↓") { action(["type": .string("scroll"), "dy": .number(3), "dx": .number(0)]) }.disabled(lease == nil)
            }.padding()
            Text(video.running ? video.status : "Snapshot mode · Capture again before input if the frame is stale. Control expires after 15 seconds. Taking control pauses competing agent input.").font(.caption2).foregroundStyle(.secondary).padding(.bottom, 8)
        }.task {
            do { guard let client = model.client else { return }; let value: JSONValue = try await client.request("v1/desktop/status"); updateDisplays(value); status = value["reason"]?.text ?? value["detail"]?.text ?? "Desktop service connected" }
            catch { status = error.localizedDescription }
        }
        .onDisappear {video.stop()}
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds:5_000_000_000)
                if video.running,lease != nil,let client=model.client {
                    do {let result:JSONValue=try await client.request("v1/desktop/lease",method:"POST",body:["controller_id":.string("human")]);lease=result["lease_id"]?.text}
                    catch{lease=nil;model.error=error.localizedDescription}
                }
            }
        }
    }
    private func updateDisplays(_ value:JSONValue){
        if case .array(let entries)=value["displays"]{displays=entries.compactMap{entry in guard let raw=entry["id"] else{return nil};let id=raw.text;guard !id.isEmpty,id != "null" else{return nil};return DesktopDisplayChoice(id:id,name:entry["name"]?.text ?? "Display \(id)")}}
        if let selected=value["selected_display_id"]?.text,displays.contains(where:{$0.id==selected}){selectedDisplayID=selected}
    }
    private func selectDisplay(_ id:String){
        guard id != selectedDisplayID,!busy else{return};let restart=video.running;video.stop();frame=nil;lease=nil;busy=true
        model.perform{defer{busy=false};guard let client=model.client else{return};try await client.mutate("v1/desktop/display",body:["display_id":.string(id)]);selectedDisplayID=id;let value:JSONValue=try await client.request("v1/desktop/status");updateDisplays(value);if restart{video.start(client)}else{frame=try await client.request("v1/desktop/frame")}}
    }
    private var displayImage:Image? {
        if video.running,let live=video.frame {return Image(decorative:live.image,scale:1)}
        guard let frame,let data=Data(base64Encoded:frame.image_base64) else{return nil};return platformImage(data)
    }
    private var displayDimensions:CGSize {
        if video.running,let live=video.frame{return CGSize(width:live.header.width,height:live.header.height)}
        return CGSize(width:frame?.width ?? 1,height:frame?.height ?? 1)
    }
    private var inputDimensions:CGSize {
        if video.running,let live=video.frame{return CGSize(width:live.header.desktop_width,height:live.header.desktop_height)}
        return displayDimensions
    }
    private func capture() { busy = true; model.perform { defer { busy = false }; guard let client = model.client else { return }; frame = try await client.request("v1/desktop/frame"); status = "\(Int(frame?.width ?? 0)) × \(Int(frame?.height ?? 0))" } }
    private func takeControl() {
        model.perform {
            guard let client = model.client else { return }
            let result: JSONValue = try await client.request("v1/desktop/lease", method: "POST", body: ["controller_id": .string("human"), "takeover": .bool(true)])
            lease = result["lease_id"]?.text; capture()
        }
    }
    private func action(_ value: [String: JSONValue]) {
        guard let lease else {return}
        let geometry:JSONValue?
        if video.running,let live=video.frame{geometry = .number(Double(live.header.geometry_version))}else{geometry=frame?.geometry_version}
        guard let geometry else{return}
        model.perform { guard let client = model.client else { return }; try await client.mutate("v1/desktop/action", body: ["lease_id": .string(lease), "controller_id": .string("human"), "geometry_version": geometry, "action": .object(value)]); if !video.running {capture()} }
    }
    private func platformImage(_ data: Data) -> Image? {
        #if os(macOS)
        guard let value = NSImage(data: data) else { return nil }; return Image(nsImage: value)
        #else
        guard let value = UIImage(data: data) else { return nil }; return Image(uiImage: value)
        #endif
    }
}

struct TeamView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var devices: [JSONValue] = []
    @State private var secret = ""
    @State private var role = "operator"
    @State private var selectedDevice: String?
    var body: some View {
        NavigationStack {
            Form {
                Section("Manage host") {
                    NavigationLink { TeamMembersView() } label: { Label("Team members", systemImage:"person.2") }
                    NavigationLink { ResourcePolicyView() } label: { Label("Resources and retention", systemImage:"externaldrive") }
                    NavigationLink { DeviceSecurityView() } label: { Label("Device security", systemImage:"lock.shield") }
                }
                PushSettingsView()
                Section("Enroll a device for your member") {
                    Picker("Role", selection: $role) { Text("Operator").tag("operator"); Text("Viewer").tag("viewer"); Text("Administrator").tag("administrator") }
                    Button("Generate single-use secret") {
                        model.perform { guard let client = model.client else { return }; let value: JSONValue = try await client.request("v1/enrollment", method: "POST", body: ["role": .string(role)]); secret = value["secret"]?.text ?? "" }
                    }
                    if !secret.isEmpty { Text(secret).font(.body.monospaced()).textSelection(.enabled); Text("Share this secret directly with the intended device owner. It expires and can only be used once.").font(.caption).foregroundStyle(.secondary) }
                }
                Section("Paired devices") {
                    ForEach(Array(devices.enumerated()), id: \.offset) { _, device in
                        HStack {
                            VStack(alignment: .leading) { Text(device["name"]?.text ?? device["device_name"]?.text ?? "Device"); Text(device["role"]?.text ?? "").font(.caption).foregroundStyle(.secondary) }
                            Spacer()
                            Button("Revoke", role: .destructive) { selectedDevice = device["id"]?.text }
                        }
                    }
                }
            }.navigationTitle("Team devices")
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .task { load() }
            .confirmationDialog("Revoke this device and close its connections?", isPresented: Binding(get: { selectedDevice != nil }, set: { if !$0 { selectedDevice = nil } }), titleVisibility: .visible) {
                Button("Revoke device", role: .destructive) { if let id = selectedDevice { model.perform { guard let client = model.client else { return }; try await client.mutate("v1/devices/\(id)/revoke"); selectedDevice = nil; load() } } }
            }
        }.frame(minWidth: 340, idealWidth: 500, minHeight: 450)
    }
    private func load() { model.perform { guard let client = model.client else { return }; devices = try await client.request("v1/devices") } }
}
