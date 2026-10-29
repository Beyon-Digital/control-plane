import SwiftUI
#if canImport(ControlPlaneKit)
import ControlPlaneKit
#endif

@MainActor final class VideoStreamModel:ObservableObject {
    @Published var frame:DecodedVideoFrame?
    @Published var status="Video stopped"
    @Published var running=false
    private var task:Task<Void,Never>?
    private var socket:URLSessionWebSocketTask?
    func start(_ client:APIClient) {
        stop();running=true
        task=Task {
            let decoder=H264VideoDecoder();var delay:UInt64=1
            while !Task.isCancelled {
                do {
                    status="Connecting video…"
                    let ws=client.socket("v1/desktop/video");socket=ws;ws.maximumMessageSize=9*1024*1024;ws.resume()
                    // A newest-only channel prevents network ingress from accumulating
                    // decoded work when a phone is slower than the host encoder.
                    let channel=AsyncThrowingStream<Data,Error>(bufferingPolicy:.bufferingNewest(1)){continuation in
                        let receiver=Task {
                            do {while !Task.isCancelled {
                                switch try await ws.receive() {
                                case .data(let data):continuation.yield(data)
                                case .string(let text):let value=try JSONDecoder().decode(JSONValue.self,from:Data(text.utf8));throw APIError.credentials(value["error"]?.text ?? "Video stream ended")
                                @unknown default:continue
                                }
                            };continuation.finish()}catch{continuation.finish(throwing:error)}
                        }
                        continuation.onTermination={_ in receiver.cancel();ws.cancel(with:.goingAway,reason:nil)}
                    }
                    for try await bytes in channel {
                        try Task.checkCancellation()
                        frame=try await decoder.decode(H264Packet(data:bytes));status="Live H264 · \(frame!.header.width) × \(frame!.header.height)";delay=1
                    }
                } catch {
                    guard !Task.isCancelled else{return}
                    status="Video unavailable · \(error.localizedDescription). Snapshots remain available. Retrying…"
                    try? await Task.sleep(nanoseconds:delay*1_000_000_000);delay=min(delay*2,15)
                }
            }
        }
    }
    func stop(){task?.cancel();socket?.cancel(with:.goingAway,reason:nil);task=nil;socket=nil;running=false;frame=nil;status="Video stopped"}
}
