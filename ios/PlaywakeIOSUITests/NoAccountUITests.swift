import XCTest

/// Launching without a Microsoft account: the PCs and Settings work, the
/// Xbox sections offer the sign-in.
final class NoAccountUITests: XCTestCase {
    private var state: URL!

    override func setUpWithError() throws {
        continueAfterFailure = false
        state = FileManager.default.temporaryDirectory.appendingPathComponent("no-account-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: state)
    }

    @MainActor
    private func launch() -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-stateDir", state.path]
        app.launch()
        return app
    }

    @MainActor
    func testWithoutSignInThePcTabWorksAndXboxOffersTheSignIn() {
        let app = launch()
        let openPcs = app.buttons["welcomeOpenPcs"]
        XCTAssertTrue(openPcs.waitForExistence(timeout: 20), "the welcome did not offer the PCs")
        XCTAssertTrue(app.buttons["Sign in with Microsoft"].exists)
        openPcs.tap()

        XCTAssertTrue(app.navigationBars["PC"].waitForExistence(timeout: 10), "the PC section did not open")
        XCTAssertFalse(app.buttons["xboxSignIn"].exists)

        app.tabBars.buttons["Consoles"].tap()
        XCTAssertTrue(app.buttons["xboxSignIn"].waitForExistence(timeout: 5), "Consoles did not offer the sign-in")
        app.tabBars.buttons["Cloud"].tap()
        XCTAssertTrue(app.buttons["xboxSignIn"].waitForExistence(timeout: 5), "Cloud did not offer the sign-in")

        app.tabBars.buttons["Settings"].tap()
        XCTAssertTrue(app.buttons["settingsSignIn"].waitForExistence(timeout: 5), "Settings did not offer the sign-in")
        let account = NSPredicate(format: "label CONTAINS 'Not signed in' OR value CONTAINS 'Not signed in'")
        XCTAssertTrue(app.descendants(matching: .any).matching(account).firstMatch.exists,
                      "Settings did not say Microsoft is not signed in")
        XCTAssertFalse(app.buttons["Sign out of Microsoft"].exists)
        attachScreenshot("no-account-settings")
    }

    /// A saved sign-in that cannot be read, with a paired PC: home at once,
    /// and the error only where Xbox is.
    @MainActor
    func testAnUnreadableSavedSignInDoesNotKeepAPairedPcAway() throws {
        try Data("{".utf8).write(to: state.appendingPathComponent("playwake-session.json"))
        let pcs = #"{"key":"","certificate":"","hosts":[{"id":"den","name":"Den","address":"192.0.2.1","certificate":""}]}"#
        try Data(pcs.utf8).write(to: state.appendingPathComponent("playwake-session.pc.json"))
        let app = launch()

        XCTAssertTrue(app.navigationBars["PC"].waitForExistence(timeout: 20), "home did not open on the PCs")
        XCTAssertFalse(app.buttons["welcomeOpenPcs"].exists)
        XCTAssertFalse(app.staticTexts["xboxError"].exists)

        app.tabBars.buttons["Consoles"].tap()
        let error = app.staticTexts["xboxError"]
        XCTAssertTrue(error.waitForExistence(timeout: 10), "Consoles did not say why Xbox is unavailable")
        XCTAssertEqual(error.label, "Could not read saved Microsoft sign-in")
        XCTAssertTrue(app.buttons["xboxSignIn"].exists)
        attachScreenshot("unreadable-sign-in")
    }

    @MainActor
    private func attachScreenshot(_ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
