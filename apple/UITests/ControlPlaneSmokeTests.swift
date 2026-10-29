import XCTest

final class ControlPlaneSmokeTests: XCTestCase {
  override func setUpWithError() throws { continueAfterFailure = false }
  @MainActor private func capture(_ name: String, app: XCUIApplication) {
    let attachment = XCTAttachment(screenshot: app.screenshot())
    attachment.name = name
    attachment.lifetime = .keepAlways
    add(attachment)
  }

  @MainActor private func openModels(_ app: XCUIApplication) {
    let chat = app.tabBars.buttons["Chat"].firstMatch
    if chat.waitForExistence(timeout: 3) { chat.tap(); capture("chat", app: app) }
    let button = app.buttons["orchestrator.models"].firstMatch
    if button.waitForExistence(timeout: 8), button.isHittable { button.tap(); return }
    let row = app.descendants(matching: .any)["navigation.models"].firstMatch
    XCTAssertTrue(row.waitForExistence(timeout: 8))
    row.tap()
  }

  @MainActor private func findModel(_ title: String, in app: XCUIApplication) -> Bool {
    let row = app.staticTexts[title].firstMatch
    for _ in 0..<8 {
      if row.waitForExistence(timeout: 1) { return true }
      app.swipeUp()
    }
    return row.exists
  }

  @MainActor func testConnectedSessionNavigation() throws {
    let environment = ProcessInfo.processInfo.environment
    guard let url = environment["CONTROL_PLANE_UI_HOST_URL"], let token = environment["CONTROL_PLANE_UI_HOST_TOKEN"], let sessionID = environment["CONTROL_PLANE_UI_SESSION_ID"] else {
      throw XCTSkip("Supply the disposable loopback UI fixture for connected navigation.")
    }
    let app = XCUIApplication()
    app.launch()
    let connect = app.buttons["host.connect"].firstMatch
    XCTAssertTrue(connect.waitForExistence(timeout: 8))
    connect.tap()
    let address = app.textFields["connection.address"]
    XCTAssertTrue(address.waitForExistence(timeout: 5))
    address.tap(); address.typeText(url)
    app.segmentedControls["connection.method"].buttons["Device token"].tap()
    let credential = app.secureTextFields["connection.token"]
    credential.tap(); credential.typeText(token)
    app.buttons["connection.submit"].tap()
    // Some simulator runtimes offer Passwords storage for previously classified
    // secure fields. The app stores device credentials in its own Keychain.
    let passwordOffer = app.buttons["Not Now"].firstMatch
    if passwordOffer.waitForExistence(timeout: 2) { passwordOffer.tap() }
    let row = app.descendants(matching: .any)["session.open." + sessionID].firstMatch
    XCTAssertTrue(row.waitForExistence(timeout: 15))
    capture("agents-connected", app: app)
    app.buttons["agents.new-session"].tap()
    XCTAssertTrue(app.textFields["session.workspace"].waitForExistence(timeout: 8))
    capture("new-session", app: app)
    app.buttons["session.create.cancel"].tap()
    row.tap()
    let panel = app.buttons["session.panel"]
    XCTAssertTrue(panel.waitForExistence(timeout: 8))
    capture("session", app: app)
    panel.tap()
    let terminal = app.descendants(matching: .any)["Terminal"].firstMatch
    XCTAssertTrue(terminal.waitForExistence(timeout: 5))
    terminal.tap()
    XCTAssertTrue(app.staticTexts["Trusted session required"].waitForExistence(timeout: 5))
    app.terminate()
  }

  @MainActor func testManualModelPersistsAfterRelaunch() throws {
    let app = XCUIApplication()
    app.launch()
    openModels(app)
    capture("models", app: app)
    let add = app.buttons["models.add"]
    XCTAssertTrue(add.waitForExistence(timeout: 8))
    add.tap()
    let name = app.textFields["model.name"]
    XCTAssertTrue(name.waitForExistence(timeout: 5))
    capture("model-editor", app: app)
    let title = "Acceptance " + String(UUID().uuidString.prefix(8))
    name.tap()
    name.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: (name.value as? String ?? "").count))
    name.typeText(title)
    let model = app.textFields["model.id"]
    if !model.isHittable { app.swipeUp() }
    model.tap()
    model.typeText("fixture-model")
    let save = app.buttons["model.save"]
    XCTAssertTrue(save.isEnabled)
    save.tap()
    XCTAssertTrue(findModel(title, in: app))
    app.terminate()
    app.launch()
    openModels(app)
    XCTAssertTrue(findModel(title, in: app))
    app.terminate()
  }

  @MainActor func testConnectionSheetCanBeCancelled() throws {
    let app = XCUIApplication()
    app.launch()
    capture("home", app: app)
    var connect = app.buttons["host.connect"].firstMatch
    if !connect.waitForExistence(timeout: 5) || !connect.isHittable {
      let back = app.navigationBars.buttons.firstMatch
      if back.exists, back.isHittable { back.tap() }
      connect = app.buttons["host.connect"].firstMatch
    }
    XCTAssertTrue(connect.waitForExistence(timeout: 8))
    connect.tap()
    let address = app.textFields["connection.address"]
    XCTAssertTrue(address.waitForExistence(timeout: 5))
    capture("connect", app: app)
    address.tap()
    let existing = address.value as? String ?? ""
    address.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: existing.count))
    address.typeText("http://127.0.0.1:9847")
    XCTAssertEqual(address.value as? String, "http://127.0.0.1:9847")
    app.buttons["connection.cancel"].tap()
    let dismissed = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: address)
    XCTAssertEqual(XCTWaiter.wait(for: [dismissed], timeout: 5), .completed)
    app.terminate()
  }
}
