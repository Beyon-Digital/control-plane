import AppKit
import ApplicationServices
import CoreGraphics
import ScreenCaptureKit

enum HelperError: Error, CustomStringConvertible {
    case message(String)
    var description: String { switch self { case .message(let value): value } }
}

@main
struct DesktopHelper {
    static func output(_ value: [String: Any]) {
        if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) {
            FileHandle.standardOutput.write(data);FileHandle.standardOutput.write(Data([10]))
        }
    }
    static func main() async {
        if CommandLine.arguments.contains("--video") || CommandLine.arguments.contains("--video-self-test") {
            do { if CommandLine.arguments.contains("--video-self-test") {try DesktopVideo.selfTest()} else {try await DesktopVideo.run()} }
            catch { FileHandle.standardError.write(Data("\(error)\n".utf8));exit(1) };return
        }
        if CommandLine.arguments.contains("--stdio-session") {
            while let line=readLine(){await handle(Data(line.utf8))};return
        }
        await handle(FileHandle.standardInput.readDataToEndOfFile())
    }
    static func handle(_ input:Data) async {
        do {
            guard input.count <= 64 * 1024,
                  let request = try JSONSerialization.jsonObject(with: input) as? [String: Any],
                  let command = request["command"] as? String else { throw HelperError.message("invalid helper request") }
            let displayID=try selectedDisplay(request)
            switch command {
            case "status":
                let capture = CGPreflightScreenCaptureAccess()
                let accessibility = AXIsProcessTrusted()
                let session = CGSessionCopyCurrentDictionary() as? [String: Any]
                let locked = session?["CGSSessionScreenIsLocked"] as? Bool ?? false
                output(["available": capture && accessibility && !locked,
                        "capture_available": capture, "input_available": accessibility && !locked,
                        "locked": locked, "platform": "macos", "accessibility": true,
                        "reason": !capture ? "Grant Screen Recording permission to the signed desktop helper in System Settings." : !accessibility ? "Grant Accessibility permission to the signed desktop helper in System Settings." : locked ? "Desktop is locked." : "Ready",
                        "selected_display_id": String(displayID), "displays": try await displays()])
            case "frame":
                try unlocked()
                guard CGPreflightScreenCaptureAccess() else { throw HelperError.message("Screen Recording permission required") }
                let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
                guard let display = content.displays.first(where: { $0.displayID == displayID }) else { throw HelperError.message("Main display unavailable") }
                let filter = SCContentFilter(display: display, excludingWindows: [])
                let config = SCStreamConfiguration()
                // Capture at logical desktop coordinates so callers can inject exact points.
                let bounds = CGDisplayBounds(display.displayID)
                config.width = Int(bounds.width)
                config.height = Int(bounds.height)
                config.showsCursor = true
                let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
                let bitmap = NSBitmapImageRep(cgImage: image)
                guard let jpeg = bitmap.representation(using: .jpeg, properties: [.compressionFactor: 0.7]) else { throw HelperError.message("JPEG encoding failed") }
                output(["image_base64": jpeg.base64EncodedString(), "mime_type": "image/jpeg",
                        "width": image.width, "height": image.height,
                        "display_id": String(display.displayID), "geometry_version": geometry(displayID: displayID)])
            case "action":
                try unlocked()
                guard AXIsProcessTrusted() else { throw HelperError.message("Accessibility permission required") }
                guard (request["geometry_version"] as? NSNumber)?.uint64Value == geometry(displayID:displayID) else { throw HelperError.message("Desktop geometry changed; capture a new frame") }
                guard let action = request["action"] as? [String: Any] else { throw HelperError.message("action required") }
                try perform(action,displayID:displayID)
                output(["success": true])
            case "select_display": output(["selected_display_id":String(displayID)])
            case "accessibility":
                try unlocked()
                guard AXIsProcessTrusted() else { throw HelperError.message("Accessibility permission required") }
                guard let app = NSWorkspace.shared.frontmostApplication else { throw HelperError.message("No foreground application") }
                let element = AXUIElementCreateApplication(app.processIdentifier)
                var remaining = 200
                output(["application": app.localizedName ?? "", "display_id":String(displayID), "coordinate_space":"global-logical", "tree": tree(element, depth: 0, remaining: &remaining)])
            default: throw HelperError.message("unsupported helper command")
            }
        } catch { output(["error": String(describing: error)]) }
    }
    static func selectedDisplay(_ request:[String:Any])throws->CGDirectDisplayID {
        guard let raw=request["display_id"] else{return CGMainDisplayID()}
        let text=(raw as? String) ?? (raw as? NSNumber)?.stringValue ?? ""
        guard let id=UInt32(text),CGDisplayIsActive(id) != 0 else{throw HelperError.message("Selected display unavailable")};return id
    }
    static func unlocked() throws {
        let session = CGSessionCopyCurrentDictionary() as? [String: Any]
        guard session != nil, session?["CGSSessionScreenIsLocked"] as? Bool != true else { throw HelperError.message("Interactive desktop is unavailable or locked") }
    }
    static func geometry(displayID:CGDirectDisplayID = CGMainDisplayID()) -> UInt64 {
        let display = displayID, bounds = CGDisplayBounds(display)
        // FNV-1a over stable integral geometry values; exactly representable in JSON.
        var hash: UInt64 = 14695981039346656037
        for value in [Int64(display), Int64(bounds.origin.x), Int64(bounds.origin.y), Int64(bounds.width), Int64(bounds.height), Int64(CGDisplayPixelsWide(display)), Int64(CGDisplayPixelsHigh(display))] {
            for byte in withUnsafeBytes(of: value.littleEndian, Array.init) { hash = (hash ^ UInt64(byte)) &* 1099511628211 }
        }
        return hash & 0x001fffffffffffff
    }
    static func displays() async throws -> [[String: Any]] {
        guard CGPreflightScreenCaptureAccess() else { return [] }
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        return content.displays.map { display in let bounds=CGDisplayBounds(display.displayID);return ["id":String(display.displayID),"name":"Display \(display.displayID)","origin_x":bounds.minX,"origin_y":bounds.minY,"width":bounds.width,"height":bounds.height,"primary":display.displayID==CGMainDisplayID()] }
    }
    static func perform(_ action: [String: Any],displayID:CGDirectDisplayID = CGMainDisplayID()) throws {
        guard let type = action["type"] as? String else { throw HelperError.message("action type required") }
        let source = CGEventSource(stateID: .hidSystemState)
        switch type {
        case "click":
            guard let x = (action["x"] as? NSNumber)?.doubleValue, let y = (action["y"] as? NSNumber)?.doubleValue else { throw HelperError.message("click coordinates required") }
            let bounds = CGDisplayBounds(displayID)
            guard x.isFinite, y.isFinite, x >= 0, y >= 0, x < bounds.width, y < bounds.height else { throw HelperError.message("click outside captured display") }
            let button: CGMouseButton = action["button"] as? String == "right" ? .right : .left
            let point = CGPoint(x: bounds.minX + x, y: bounds.minY + y)
            let down: CGEventType = button == .right ? .rightMouseDown : .leftMouseDown
            let up: CGEventType = button == .right ? .rightMouseUp : .leftMouseUp
            guard let a = CGEvent(mouseEventSource: source, mouseType: down, mouseCursorPosition: point, mouseButton: button), let b = CGEvent(mouseEventSource: source, mouseType: up, mouseCursorPosition: point, mouseButton: button) else { throw HelperError.message("Cannot create mouse event") }
            a.post(tap: .cghidEventTap); b.post(tap: .cghidEventTap)
        case "text":
            guard let text = action["text"] as? String, text.utf8.count <= 4096 else { throw HelperError.message("text input exceeds limit") }
            let units = Array(text.utf16)
            for offset in stride(from: 0, to: units.count, by: 20) {
                let chunk = Array(units[offset..<min(offset + 20, units.count)])
                guard let event = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true), let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else { throw HelperError.message("Cannot create text event") }
                event.keyboardSetUnicodeString(stringLength: chunk.count, unicodeString: chunk)
                event.post(tap: .cghidEventTap); up.post(tap: .cghidEventTap)
            }
        case "key":
            let keys: [String: CGKeyCode] = ["enter":36,"return":36,"tab":48,"space":49,"backspace":51,"escape":53,"left":123,"right":124,"down":125,"up":126,"delete":117,"home":115,"end":119,"pageup":116,"pagedown":121,"a":0,"c":8,"v":9,"x":7,"z":6,"s":1]
            guard let name = action["key"] as? String, let code = keys[name.lowercased()] else { throw HelperError.message("Unsupported key") }
            var flags: CGEventFlags = []
            let modifiers = action["modifiers"] as? [String] ?? []
            guard modifiers.count <= 4 else { throw HelperError.message("too many modifiers") }
            for modifier in modifiers {
                switch modifier { case "command": flags.insert(.maskCommand); case "control": flags.insert(.maskControl); case "option": flags.insert(.maskAlternate); case "shift": flags.insert(.maskShift); default: throw HelperError.message("unsupported modifier") }
            }
            guard let down = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: true), let up = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: false) else { throw HelperError.message("Cannot create key event") }
            down.flags = flags; up.flags = flags
            down.post(tap: .cghidEventTap); up.post(tap: .cghidEventTap)
        case "scroll":
            guard let dx = ((action["dx"] ?? action["delta_x"]) as? NSNumber)?.int32Value, let dy = ((action["dy"] ?? action["delta_y"]) as? NSNumber)?.int32Value, abs(Int64(dx)) <= 1200, abs(Int64(dy)) <= 1200 else { throw HelperError.message("invalid scroll distances") }
            guard let event = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2, wheel1: dy, wheel2: dx, wheel3: 0) else { throw HelperError.message("Cannot create scroll event") }
            event.post(tap: .cghidEventTap)
        default: throw HelperError.message("unsupported action")
        }
    }
    static func tree(_ element: AXUIElement, depth: Int, remaining: inout Int) -> [String: Any] {
        guard depth < 6, remaining > 0 else { return ["truncated": true] }
        remaining -= 1
        var subrole:CFTypeRef?
        if AXUIElementCopyAttributeValue(element,kAXSubroleAttribute as CFString,&subrole) == .success,(subrole as? String)==kAXSecureTextFieldSubrole{return ["role":"secure-text-field","password":true]}
        var result: [String: Any] = [:]
        for (key, attribute) in [("role", kAXRoleAttribute), ("title", kAXTitleAttribute), ("description", kAXDescriptionAttribute)] {
            var value: CFTypeRef?
            if AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success, let string = value as? String { result[key] = String(string.prefix(512)) }
        }
        var children: CFTypeRef?
        if AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &children) == .success, let values = children as? [AXUIElement] {
            result["children"] = values.prefix(40).map { tree($0, depth: depth + 1, remaining: &remaining) }
        }
        return result
    }
}
