import Foundation
import SwiftUI

struct DiscoveredHost: Identifiable, Hashable {
  let id: String
  let name: String
  let address: String
}
@MainActor
final class HostDiscovery: NSObject, ObservableObject, @preconcurrency NetServiceBrowserDelegate,
  @preconcurrency NetServiceDelegate
{
  @Published private(set) var hosts: [DiscoveredHost] = []
  @Published private(set) var searching = false
  @Published private(set) var status = "Search your local network for private coding hosts."
  private let browser = NetServiceBrowser()
  private var services: [String: NetService] = [:]
  override init() {
    super.init()
    browser.delegate = self
  }
  func start() {
    guard !searching else { return }
    hosts = []
    services = [:]
    searching = true
    status = "Searching local network…"
    browser.searchForServices(ofType: "_control-plane._tcp.", inDomain: "local.")
  }
  func stop() {
    browser.stop()
    for service in services.values {
      service.stop()
      service.delegate = nil
    }
    services = [:]
    searching = false
  }
  private func key(_ service: NetService) -> String {
    service.name + "|" + service.type + "|" + service.domain
  }
  func netServiceBrowser(
    _ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool
  ) {
    let id = key(service)
    services[id] = service
    service.delegate = self
    service.resolve(withTimeout: 5)
  }
  func netServiceBrowser(
    _ browser: NetServiceBrowser, didRemove service: NetService, moreComing: Bool
  ) {
    let id = key(service)
    services.removeValue(forKey: id)?.stop()
    hosts.removeAll { $0.id == id }
  }
  func netServiceBrowserDidStopSearch(_ browser: NetServiceBrowser) { searching = false }
  func netServiceBrowser(_ browser: NetServiceBrowser, didNotSearch errorDict: [String: NSNumber]) {
    searching = false
    status =
      "Local discovery is unavailable. Check Local Network permission in Settings, or enter the host address manually."
  }
  func netServiceDidResolveAddress(_ sender: NetService) {
    guard let hostname = sender.hostName, sender.port > 0 else { return }
    let txt = sender.txtRecordData().map { NetService.dictionary(fromTXTRecord: $0) } ?? [:]
    guard txt["tls"].flatMap({ String(data: $0, encoding: .utf8) }) == "1" else { return }
    let rawPath = txt["path"].flatMap { String(data: $0, encoding: .utf8) } ?? ""
    let path = rawPath.isEmpty || rawPath == "/" ? "" : rawPath
    guard
      path.isEmpty
        || (path.hasPrefix("/accounts/") && !path.contains("..")
          && path.utf8.allSatisfy {
            (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0)
              || [45, 47, 95].contains($0)
          })
    else { return }
    var url = URLComponents()
    url.scheme = "https"
    url.host = hostname.hasSuffix(".") ? String(hostname.dropLast()) : hostname
    url.port = sender.port
    url.path = path
    guard let address = url.url?.absoluteString else { return }
    let host = DiscoveredHost(id: key(sender), name: sender.name, address: address)
    hosts.removeAll { $0.id == host.id }
    hosts.append(host)
    hosts.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    status = "Discovered hosts still require a trusted certificate and explicit device pairing."
  }
  func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
    services.removeValue(forKey: key(sender))
    status = "A host could not be resolved. You can enter its address manually."
  }
}
