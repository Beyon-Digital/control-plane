import Foundation
import CoreMedia
import CoreVideo
import CoreImage
import VideoToolbox

public struct VideoHeader: Codable, Sendable, Equatable {
    public let version:Int
    public let codec:String
    public let width:Int
    public let height:Int
    public let desktop_width:Int
    public let desktop_height:Int
    public let geometry_version:UInt64
    public let timestamp_us:UInt64
    public let keyframe:Bool
    public let sps_base64:String
    public let pps_base64:String
}
public struct H264Packet: Sendable {
    public let header:VideoHeader
    public let payload:Data
    public init(data:Data) throws {
        guard data.count>=12,data.prefix(4)==Data("CPV1".utf8) else {throw APIError.invalidResponse}
        func integer(_ offset:Int)->Int{data[offset..<offset+4].reduce(0){($0<<8)|Int($1)}}
        let h=integer(4),p=integer(8)
        guard h>0,h<=65536,p>0,p<=8*1024*1024,data.count==12+h+p else {throw APIError.invalidResponse}
        header=try JSONDecoder().decode(VideoHeader.self,from:data.subdata(in:12..<12+h))
        guard header.version==1,header.codec=="h264",header.keyframe,header.width>0,header.height>0,header.width<=4096,header.height<=4096,header.desktop_width>0,header.desktop_height>0,header.desktop_width<=32768,header.desktop_height<=32768 else {throw APIError.invalidResponse}
        payload=data.subdata(in:12+h..<data.count)
        var offset=0;var idr=false
        while offset<payload.count {
            guard offset+4<=payload.count else {throw APIError.invalidResponse}
            let length=payload[offset..<offset+4].reduce(0){($0<<8)|Int($1)};offset+=4
            guard length>0,length<=payload.count-offset else {throw APIError.invalidResponse}
            if payload[offset]&0x1f==5 {idr=true};offset+=length
        }
        guard idr else {throw APIError.invalidResponse}
    }
}
public struct DecodedVideoFrame: @unchecked Sendable {
    public let image:CGImage
    public let header:VideoHeader
}
private final class DecodeOutput {
    var pixel:CVPixelBuffer?
    var status:OSStatus=noErr
}
/// Serial decoder actor; never retains a history of compressed or decoded frames.
public actor H264VideoDecoder {
    private var session:VTDecompressionSession?
    private var format:CMVideoFormatDescription?
    private var parameters=Data()
    private let images=CIContext(options:[.cacheIntermediates:false])
    public init() {}
    public func decode(_ packet:H264Packet) throws -> DecodedVideoFrame {
        guard let sps=Data(base64Encoded:packet.header.sps_base64),let pps=Data(base64Encoded:packet.header.pps_base64),sps.count>0,pps.count>0,sps.first!&0x1f==7,pps.first!&0x1f==8 else {throw APIError.invalidResponse}
        let config=sps+pps
        if parameters != config || session == nil {
            if let session {VTDecompressionSessionInvalidate(session)}
            session=nil;format=nil
            let status=sps.withUnsafeBytes { spsBytes in pps.withUnsafeBytes { ppsBytes in
                let pointers=[spsBytes.baseAddress!.assumingMemoryBound(to:UInt8.self),ppsBytes.baseAddress!.assumingMemoryBound(to:UInt8.self)]
                let sizes=[sps.count,pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(allocator:kCFAllocatorDefault,parameterSetCount:2,parameterSetPointers:pointers,parameterSetSizes:sizes,nalUnitHeaderLength:4,formatDescriptionOut:&format)
            }}
            guard status==noErr,let format else {throw APIError.credentials("Invalid H264 decoder configuration (\(status)).")}
            var callback=VTDecompressionOutputCallbackRecord(decompressionOutputCallback:{_,source,status,_,image,_,_ in
                guard let source else{return};let output=Unmanaged<DecodeOutput>.fromOpaque(source).takeUnretainedValue();output.status=status;output.pixel=image
            },decompressionOutputRefCon:nil)
            let created=VTDecompressionSessionCreate(allocator:kCFAllocatorDefault,formatDescription:format,decoderSpecification:nil,imageBufferAttributes:[kCVPixelBufferPixelFormatTypeKey:kCVPixelFormatType_32BGRA,kCVPixelBufferIOSurfacePropertiesKey:[:]] as CFDictionary,outputCallback:&callback,decompressionSessionOut:&session)
            guard created==noErr else{throw APIError.credentials("Cannot create video decoder (\(created)).")};parameters=config
        }
        guard let session,let format else{throw APIError.invalidResponse}
        var block:CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(allocator:kCFAllocatorDefault,memoryBlock:nil,blockLength:packet.payload.count,blockAllocator:kCFAllocatorDefault,customBlockSource:nil,offsetToData:0,dataLength:packet.payload.count,flags:0,blockBufferOut:&block)==noErr,let block else{throw APIError.invalidResponse}
        let copied=packet.payload.withUnsafeBytes{CMBlockBufferReplaceDataBytes(with:$0.baseAddress!,blockBuffer:block,offsetIntoDestination:0,dataLength:packet.payload.count)}
        guard copied==noErr else{throw APIError.invalidResponse}
        var timing=CMSampleTimingInfo(duration:CMTime(value:1,timescale:30),presentationTimeStamp:CMTime(value:Int64(clamping:packet.header.timestamp_us),timescale:1_000_000),decodeTimeStamp:.invalid)
        var sample:CMSampleBuffer?;var size=packet.payload.count
        guard CMSampleBufferCreateReady(allocator:kCFAllocatorDefault,dataBuffer:block,formatDescription:format,sampleCount:1,sampleTimingEntryCount:1,sampleTimingArray:&timing,sampleSizeEntryCount:1,sampleSizeArray:&size,sampleBufferOut:&sample)==noErr,let sample else{throw APIError.invalidResponse}
        let output=DecodeOutput();var flags=VTDecodeInfoFlags()
        let decoded=VTDecompressionSessionDecodeFrame(session,sampleBuffer:sample,flags:[],frameRefcon:Unmanaged.passUnretained(output).toOpaque(),infoFlagsOut:&flags)
        VTDecompressionSessionWaitForAsynchronousFrames(session)
        guard decoded==noErr,output.status==noErr,let pixel=output.pixel else{throw APIError.credentials("H264 frame decode failed (\(decoded), \(output.status)).")}
        let image=CIImage(cvPixelBuffer:pixel)
        guard let cgImage=images.createCGImage(image,from:image.extent) else{throw APIError.invalidResponse}
        return DecodedVideoFrame(image:cgImage,header:packet.header)
    }
}
