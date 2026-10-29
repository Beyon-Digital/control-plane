import XCTest
#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif
final class TerminalWireTests:XCTestCase{
 func testBinaryTerminalOutputPreservesPartialUTF8AndFullCursor()throws{let bytes=Data([27,91,51,49,109,0xCE]);let data=try JSONSerialization.data(withJSONObject:["type":"output","sequence":UInt64.max,"data":bytes.base64EncodedString()]);let frame=try JSONDecoder().decode(TerminalWireMessage.self,from:data);XCTAssertEqual(frame.sequence,UInt64.max);XCTAssertEqual(try frame.outputBytes(),bytes)}
 func testMalformedAndOversizedPayloadsRejected()throws{for encoded in ["not base64!",Data(repeating:0,count:65537).base64EncodedString()]{let data=try JSONSerialization.data(withJSONObject:["type":"output","sequence":1,"data":encoded]);let frame=try JSONDecoder().decode(TerminalWireMessage.self,from:data);XCTAssertThrowsError(try frame.outputBytes())}}
 func testGapRequiresNoData()throws{let frame=try JSONDecoder().decode(TerminalWireMessage.self,from:Data(#"{"type":"gap","sequence":100,"message":"expired"}"#.utf8));XCTAssertEqual(frame.sequence,100);XCTAssertThrowsError(try frame.outputBytes())}
}
