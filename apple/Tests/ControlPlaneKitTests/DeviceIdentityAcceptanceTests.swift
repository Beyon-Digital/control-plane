import XCTest
import Security
#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif
private struct CSRReader{
 let bytes:Data;var offset=0
 mutating func next()throws->(UInt8,Data,Data){let start=offset;guard offset+2<=bytes.count else{throw APIError.invalidResponse};let tag=bytes[offset];offset+=1;let first=Int(bytes[offset]);offset+=1;var length=first;if first&128 != 0{let count=first&127;guard count>0,count<=4,offset+count<=bytes.count else{throw APIError.invalidResponse};length=0;for _ in 0..<count{length=(length<<8)|Int(bytes[offset]);offset+=1};guard length>=128 else{throw APIError.invalidResponse}};guard length<=65536,offset+length<=bytes.count else{throw APIError.invalidResponse};let content=bytes.subdata(in:offset..<offset+length);offset+=length;return(tag,content,bytes.subdata(in:start..<offset))}
}
final class DeviceIdentityAcceptanceTests:XCTestCase{
 func testCSRPublicKeyAndSelfSignature()throws{let attrs:[String:Any]=[kSecAttrKeyType as String:kSecAttrKeyTypeECSECPrimeRandom,kSecAttrKeySizeInBits as String:256];let key=try XCTUnwrap(SecKeyCreateRandomKey(attrs as CFDictionary,nil)),publicKey=try XCTUnwrap(SecKeyCopyPublicKey(key));let pem=try DeviceIdentity.csr(privateKey:key);let bytes=try XCTUnwrap(Data(base64Encoded:pem.components(separatedBy:.newlines).filter{!$0.hasPrefix("-----")}.joined()));var top=CSRReader(bytes:bytes);let sequence=try top.next();XCTAssertEqual(sequence.0,48);var fields=CSRReader(bytes:sequence.1);let info=try fields.next(),algorithm=try fields.next(),signature=try fields.next();XCTAssertEqual(algorithm.1,Data([6,8,0x2a,0x86,0x48,0xce,0x3d,4,3,2]));XCTAssertEqual(signature.1.first,0);XCTAssertTrue(SecKeyVerifySignature(publicKey,.ecdsaSignatureMessageX962SHA256,info.2 as CFData,Data(signature.1.dropFirst()) as CFData,nil));var request=CSRReader(bytes:info.1);_ = try request.next();_ = try request.next();let spki=try request.next();var bits=CSRReader(bytes:spki.1);_ = try bits.next();let value=try bits.next();XCTAssertEqual(Data(value.1.dropFirst()),SecKeyCopyExternalRepresentation(publicKey,nil) as Data?)}
 func testMalformedDERRejected(){for data in [Data([48,128]),Data([48,130,255]),Data([48,3,1])]{var reader=CSRReader(bytes:data);XCTAssertThrowsError(try reader.next())}}
}
