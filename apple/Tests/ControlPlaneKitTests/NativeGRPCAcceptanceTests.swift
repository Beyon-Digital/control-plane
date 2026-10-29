import XCTest
import CryptoKit
import Security
#if CONTROL_PLANE_XCODE_TESTS
@testable import ControlPlane
#else
@testable import ControlPlaneKit
#endif
final class NativeGRPCAcceptanceTests:XCTestCase{
 func testNativeHTTP2ReadsRealRustHost()async throws{
  let env=ProcessInfo.processInfo.environment
  guard let address=env["CONTROL_PLANE_TEST_URL"],let rpc=env["CONTROL_PLANE_TEST_GRPC_URL"],let token=env["CONTROL_PLANE_TEST_TOKEN"],let url=URL(string:address) else{throw XCTSkip("Set disposable Rust fixture URLs and token for native HTTP2 acceptance.")}
  var credential = token
  let scope = try APIClient(address: address, token: "", grpcAddress: "").baseURL.absoluteString
  var fixtureIdentity = false
  defer {
   if fixtureIdentity {
    let label = "dev.controlplane.device." + SHA256.hash(data: Data(scope.utf8)).map { String(format: "%02x", $0) }.joined()
    SecItemDelete([kSecClass as String:kSecClassCertificate,kSecAttrLabel as String:label] as CFDictionary)
    SecItemDelete([kSecClass as String:kSecClassKey,kSecAttrApplicationTag as String:Data(label.utf8),kSecAttrKeyType as String:kSecAttrKeyTypeECSECPrimeRandom] as CFDictionary)
    CredentialStore.delete(host:"device-identity:"+scope)
    CredentialStore.delete(host:"host-ca:"+scope)
   }
  }
  if url.scheme == "https" {
   guard let ca=env["CONTROL_PLANE_TEST_CA"],let fingerprint=env["CONTROL_PLANE_TEST_CA_SHA256"],let secret=env["CONTROL_PLANE_TEST_ENROLLMENT"] else {throw APIError.invalidAddress}
   fixtureIdentity = true
   try DeviceIdentity.trustHostCA(ca,host:scope,expectedSHA256:fingerprint)
   let paired = try await DeviceIdentity.pair(using:APIClient(address:address,token:"",grpcAddress:""),secret:secret,name:"iPhone acceptance",invitation:false)
   credential = paired.token
   XCTAssertNotNil(try DeviceIdentity.identity(host:scope))
  } else {
   guard url.scheme=="http",["localhost","127.0.0.1"].contains(url.host ?? "") else{throw APIError.invalidAddress}
  }
  let http=try APIClient(address:address,token:credential,grpcAddress:"");let grpc=try NativeGRPCClient(address:rpc,httpBaseURL:http.baseURL,token:credential)
  let expected:HostInfo=try await http.request("v1/host");let data=try await grpc.request(path:"v1/host",method:"GET",query:[:],body:nil);let actual=try JSONDecoder().decode(HostInfo.self,from:data);XCTAssertEqual(actual.id,expected.id)
  let sessions=try JSONDecoder().decode([AgentSession].self,from:await grpc.request(path:"v1/sessions",method:"GET",query:[:],body:nil))
  guard let id=sessions.first?.id else{throw XCTSkip("Precreate an idle fixture session for replay.")}
  let source=FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
  let binary=Data([0,255,128,0,27,91,1,2,3]);try binary.write(to:source);defer{try? FileManager.default.removeItem(at:source)}
  let path="fixture-"+UUID().uuidString+".bin"
  _ = try await http.uploadFile(from:source,sessionID:id,path:path,replaceExisting:false)
  let download=try await http.downloadFile(sessionID:id,path:path);defer{try? FileManager.default.removeItem(at:download)}
  XCTAssertEqual(try Data(contentsOf:download),binary)
  try Data([3,2,1,0,255]).write(to:source)
  _ = try await http.uploadFile(from:source,sessionID:id,path:path,replaceExisting:true)
  let stream=grpc.events(sessionID:id,after:0)
  let event=try await withThrowingTaskGroup(of:SessionEvent.self){group in group.addTask{for try await value in stream{return value};throw APIError.invalidResponse};group.addTask{try await Task.sleep(nanoseconds:3_000_000_000);throw APIError.server(504,"gRPC replay timed out")};defer{group.cancelAll()};return try await group.next()!}
  XCTAssertEqual(event.session_id,id)
 }
 func testCursorIsExactBeyondDoublePrecision()throws{let event=try JSONDecoder().decode(SessionEvent.self,from:Data(#"{"sequence":9007199254740993,"session_id":"fixture","kind":"state","payload":{},"created_at":1}"#.utf8));XCTAssertEqual(event.sequence,9_007_199_254_740_993)}
}
