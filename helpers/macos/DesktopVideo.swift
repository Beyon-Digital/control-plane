import Foundation
import AppKit
import ScreenCaptureKit
import VideoToolbox
import CoreMedia
import CoreVideo

/// One outstanding encode and a bounded ScreenCaptureKit queue. Every output
/// is an IDR access unit, independently decodable after transport frame drops.
final class DesktopVideoEncoder: @unchecked Sendable {
    private var session: VTCompressionSession?
    private let lock = NSLock()
    private var pending = false
    private var failure: String?
    let width: Int
    let height: Int
    let desktopWidth: Int
    let desktopHeight: Int
    var displayID:CGDirectDisplayID = CGMainDisplayID()
    let geometry: UInt64
    init(width: Int, height: Int, desktopWidth: Int, desktopHeight: Int, geometry: UInt64) throws {
        self.width=width;self.height=height;self.desktopWidth=desktopWidth;self.desktopHeight=desktopHeight;self.geometry=geometry
        let callback: VTCompressionOutputCallback = { reference, _, status, _, sample in
            guard let reference else { return }
            let encoder=Unmanaged<DesktopVideoEncoder>.fromOpaque(reference).takeUnretainedValue()
            defer { encoder.lock.lock();encoder.pending=false;encoder.lock.unlock() }
            guard status == noErr, let sample else { encoder.fail("VideoToolbox compression failed (\(status))");return }
            do { try encoder.output(sample) } catch { encoder.fail(error.localizedDescription) }
        }
        let status=VTCompressionSessionCreate(allocator:kCFAllocatorDefault,width:Int32(width),height:Int32(height),codecType:kCMVideoCodecType_H264,encoderSpecification:[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder:true] as CFDictionary,imageBufferAttributes:nil,compressedDataAllocator:nil,outputCallback:callback,refcon:Unmanaged.passUnretained(self).toOpaque(),compressionSessionOut:&session)
        guard status == noErr, let session else { throw HelperError.message("Cannot create H264 encoder (\(status))") }
        let properties: [CFString:Any] = [kVTCompressionPropertyKey_RealTime:true,kVTCompressionPropertyKey_AllowFrameReordering:false,kVTCompressionPropertyKey_ProfileLevel:kVTProfileLevel_H264_Baseline_AutoLevel,kVTCompressionPropertyKey_AverageBitRate:8_000_000,kVTCompressionPropertyKey_ExpectedFrameRate:30,kVTCompressionPropertyKey_MaxKeyFrameInterval:1,kVTCompressionPropertyKey_MaxFrameDelayCount:1]
        guard VTSessionSetProperties(session,propertyDictionary:properties as CFDictionary)==noErr,
              VTCompressionSessionPrepareToEncodeFrames(session)==noErr else { throw HelperError.message("Cannot configure low-latency H264 encoder") }
    }
    deinit { if let session { VTCompressionSessionInvalidate(session) } }
    func encode(_ image: CVPixelBuffer, timestamp: CMTime) {
        lock.lock();if pending || failure != nil { lock.unlock();return };pending=true;lock.unlock()
        guard let session else { return }
        var flags=VTEncodeInfoFlags()
        let result=VTCompressionSessionEncodeFrame(session,imageBuffer:image,presentationTimeStamp:timestamp,duration:CMTime(value:1,timescale:30),frameProperties:[kVTEncodeFrameOptionKey_ForceKeyFrame:true] as CFDictionary,sourceFrameRefcon:nil,infoFlagsOut:&flags)
        if result != noErr { lock.lock();pending=false;lock.unlock();fail("H264 encoding failed (\(result))") }
    }
    func complete() { if let session { VTCompressionSessionCompleteFrames(session,untilPresentationTimeStamp:.invalid) } }
    func error() -> String? { lock.lock();defer{lock.unlock()};return failure }
    private func fail(_ error: String) { lock.lock();failure=error;lock.unlock() }
    private func output(_ sample: CMSampleBuffer) throws {
        guard let format=CMSampleBufferGetFormatDescription(sample),let block=CMSampleBufferGetDataBuffer(sample) else { throw HelperError.message("Encoded frame has no format or payload") }
        func parameter(_ index:Int) throws -> Data {
            var pointer:UnsafePointer<UInt8>?;var length=0;var count=0;var header:Int32=0
            guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,parameterSetIndex:index,parameterSetPointerOut:&pointer,parameterSetSizeOut:&length,parameterSetCountOut:&count,nalUnitHeaderLengthOut:&header)==noErr,let pointer,length>0,header==4 else { throw HelperError.message("Missing H264 parameter set") }
            return Data(bytes:pointer,count:length)
        }
        let sps=try parameter(0),pps=try parameter(1)
        var pointer:UnsafeMutablePointer<Int8>?;var length=0
        guard CMBlockBufferGetDataPointer(block,atOffset:0,lengthAtOffsetOut:nil,totalLengthOut:&length,dataPointerOut:&pointer)==noErr,let pointer,length>0,length<=8*1024*1024 else { throw HelperError.message("Invalid H264 access unit") }
        let pts=CMSampleBufferGetPresentationTimeStamp(sample)
        let micros=CMTimeConvertScale(pts,timescale:1_000_000,method:.default).value
        let header:[String:Any]=["version":1,"codec":"h264","width":width,"height":height,"desktop_width":desktopWidth,"desktop_height":desktopHeight,"geometry_version":geometry,"timestamp_us":max(0,micros),"display_id":String(displayID),"keyframe":true,"sps_base64":sps.base64EncodedString(),"pps_base64":pps.base64EncodedString()]
        let json=try JSONSerialization.data(withJSONObject:header,options:[.sortedKeys])
        var packet=Data("CPV1".utf8)
        var headerLength=UInt32(json.count).bigEndian;var payloadLength=UInt32(length).bigEndian
        withUnsafeBytes(of:&headerLength){packet.append(contentsOf:$0)};withUnsafeBytes(of:&payloadLength){packet.append(contentsOf:$0)}
        packet.append(json);packet.append(Data(bytes:pointer,count:length))
        FileHandle.standardOutput.write(packet)
    }
}

final class DesktopCaptureOutput: NSObject,SCStreamOutput,SCStreamDelegate,@unchecked Sendable {
    let encoder:DesktopVideoEncoder
    private var lastImage:CVPixelBuffer?
    private var stopped:String?
    private let statusLock=NSLock()
    init(_ encoder:DesktopVideoEncoder){self.encoder=encoder}
    func stream(_ stream:SCStream,didOutputSampleBuffer sampleBuffer:CMSampleBuffer,of type:SCStreamOutputType){
        guard type == .screen,CMSampleBufferIsValid(sampleBuffer) else{return}
        let attachments=CMSampleBufferGetSampleAttachmentsArray(sampleBuffer,createIfNecessary:false) as? [[SCStreamFrameInfo:Any]]
        let state=(attachments?.first?[.status] as? Int).flatMap(SCFrameStatus.init(rawValue:)) ?? .complete
        if state == .complete,let image=CMSampleBufferGetImageBuffer(sampleBuffer){lastImage=image}
        // An idle callback explicitly confirms that the current image is unchanged.
        // Re-encode that image to keep input freshness meaningful on static screens.
        guard state == .complete || state == .idle,let image=lastImage else{return}
        encoder.encode(image,timestamp:CMSampleBufferGetPresentationTimeStamp(sampleBuffer))
    }
    func stream(_ stream:SCStream,didStopWithError error:Error){statusLock.lock();stopped=error.localizedDescription;statusLock.unlock()}
    func error()->String?{statusLock.lock();defer{statusLock.unlock()};return stopped}
}

enum DesktopVideo {
    static func run() async throws {
        try DesktopHelper.unlocked()
        guard CGPreflightScreenCaptureAccess() else { throw HelperError.message("Screen Recording permission required") }
        let content=try await SCShareableContent.excludingDesktopWindows(false,onScreenWindowsOnly:true)
        var request:[String:Any]=[:]
        if let index=CommandLine.arguments.firstIndex(of:"--display-id"),index+1<CommandLine.arguments.count{request["display_id"]=CommandLine.arguments[index+1]}
        let displayID=try DesktopHelper.selectedDisplay(request)
        guard let display=content.displays.first(where:{$0.displayID==displayID}) else {throw HelperError.message("Main display unavailable")}
        let bounds=CGDisplayBounds(display.displayID)
        let scale=min(1.0,min(1920/bounds.width,1080/bounds.height))
        let width=max(2,Int(bounds.width*scale)/2*2),height=max(2,Int(bounds.height*scale)/2*2)
        let geometry=DesktopHelper.geometry(displayID:displayID)
        let encoder=try DesktopVideoEncoder(width:width,height:height,desktopWidth:Int(bounds.width),desktopHeight:Int(bounds.height),geometry:geometry)
        encoder.displayID=displayID
        let output=DesktopCaptureOutput(encoder)
        let config=SCStreamConfiguration();config.width=width;config.height=height;config.pixelFormat=kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;config.minimumFrameInterval=CMTime(value:1,timescale:30);config.queueDepth=3;config.showsCursor=true
        let stream=SCStream(filter:SCContentFilter(display:display,excludingWindows:[]),configuration:config,delegate:output)
        try stream.addStreamOutput(output,type:.screen,sampleHandlerQueue:DispatchQueue(label:"dev.controlplane.screen",qos:.userInteractive))
        try await stream.startCapture()
        do {
            while true {
                try await Task.sleep(nanoseconds:200_000_000)
                try DesktopHelper.unlocked()
                guard DesktopHelper.geometry(displayID:displayID)==geometry else{throw HelperError.message("Display geometry changed; reconnect the video stream")}
                if let error=encoder.error(){throw HelperError.message(error)}
                if let error=output.error(){throw HelperError.message(error)}
            }
        } catch { try? await stream.stopCapture();encoder.complete();throw error }
    }
    /// A real VideoToolbox encode fixture with synthetic pixels, no screen access.
    static func selfTest() throws {
        let encoder=try DesktopVideoEncoder(width:64,height:64,desktopWidth:64,desktopHeight:64,geometry:1)
        var pixel:CVPixelBuffer?
        guard CVPixelBufferCreate(kCFAllocatorDefault,64,64,kCVPixelFormatType_32BGRA,[kCVPixelBufferIOSurfacePropertiesKey:[:]] as CFDictionary,&pixel)==kCVReturnSuccess,let pixel else{throw HelperError.message("Synthetic pixel buffer failed")}
        CVPixelBufferLockBaseAddress(pixel,[])
        if let pointer=CVPixelBufferGetBaseAddress(pixel){memset(pointer,100,CVPixelBufferGetBytesPerRow(pixel)*64)}
        CVPixelBufferUnlockBaseAddress(pixel,[])
        encoder.encode(pixel,timestamp:CMTime(value:1,timescale:30));encoder.complete()
        if let error=encoder.error(){throw HelperError.message(error)}
    }
}
