import XCTest
#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif
#if os(macOS)
import SwiftTerm
import AppKit
final class NativeTerminalEmulationTests:XCTestCase{
 @MainActor private func terminal()->SwiftTerm.TerminalView{_ = NSApplication.shared;let view=SwiftTerm.TerminalView(frame:CGRect(x:0,y:0,width:800,height:480));view.changeScrollback(2000);view.resize(cols:80,rows:24);return view}
 @MainActor func testCursorAndAlternateScreen(){let v=terminal();v.feed(byteArray:Array("hello\rXX".utf8)[...]);XCTAssertTrue(String(decoding:v.getTerminal().getBufferAsData(),as:UTF8.self).contains("XXllo"));v.feed(byteArray:Array("\u{1b}[?1049hALTERNATE".utf8)[...]);XCTAssertTrue(v.getTerminal().isCurrentBufferAlternate);XCTAssertFalse(String(decoding:v.getTerminal().getBufferAsData(),as:UTF8.self).contains("XXllo"));v.feed(byteArray:Array("\u{1b}[?1049l".utf8)[...]);XCTAssertFalse(v.getTerminal().isCurrentBufferAlternate);XCTAssertTrue(String(decoding:v.getTerminal().getBufferAsData(),as:UTF8.self).contains("XXllo"))}
 @MainActor func testPartialUTF8AcrossFrames()throws{let v=terminal();for b in [Data([0xCE]),Data([0xBB])]{let frame=try JSONDecoder().decode(TerminalWireMessage.self,from:JSONSerialization.data(withJSONObject:["type":"output","sequence":1,"data":b.base64EncodedString()]));v.feed(byteArray:Array(try frame.outputBytes())[...])};let text=String(decoding:v.getTerminal().getBufferAsData(),as:UTF8.self);XCTAssertTrue(text.contains("λ"));XCTAssertFalse(text.contains("�"))}
 @MainActor func testResizeUsesActualCells(){let v=terminal();let d=TerminalRecordingDelegate();v.terminalDelegate=d;v.resize(cols:120,rows:40);XCTAssertEqual(v.getTerminal().cols,120);XCTAssertEqual(v.getTerminal().rows,40);XCTAssertEqual(d.sizes.last?.0,120);XCTAssertEqual(d.sizes.last?.1,40)}
 @MainActor func testColorAndEraseInterpreted(){let v=terminal();v.feed(byteArray:Array("old content\r\u{1b}[2K\u{1b}[31mred\u{1b}[0m".utf8)[...]);let text=String(decoding:v.getTerminal().getBufferAsData(),as:UTF8.self);XCTAssertTrue(text.contains("red"));XCTAssertFalse(text.contains("old content"));XCTAssertFalse(text.contains("[31m"))}
}
private final class TerminalRecordingDelegate:SwiftTerm.TerminalViewDelegate{
 var sizes:[(Int,Int)]=[]
 func sizeChanged(source:SwiftTerm.TerminalView,newCols:Int,newRows:Int){sizes.append((newCols,newRows))}
 func send(source:SwiftTerm.TerminalView,data:ArraySlice<UInt8>){}
 func setTerminalTitle(source:SwiftTerm.TerminalView,title:String){}
 func hostCurrentDirectoryUpdate(source:SwiftTerm.TerminalView,directory:String?){}
 func scrolled(source:SwiftTerm.TerminalView,position:Double){}
 func requestOpenLink(source:SwiftTerm.TerminalView,link:String,params:[String:String]){}
 func bell(source:SwiftTerm.TerminalView){}
 func clipboardCopy(source:SwiftTerm.TerminalView,content:Data){}
 func clipboardRead(source:SwiftTerm.TerminalView)->Data?{nil}
 func iTermContent(source:SwiftTerm.TerminalView,content:ArraySlice<UInt8>){}
 func rangeChanged(source:SwiftTerm.TerminalView,startY:Int,endY:Int){}
}
#endif
