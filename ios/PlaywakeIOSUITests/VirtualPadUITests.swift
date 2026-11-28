import XCTest

/// The system on-screen pad; Playwake's own is `TouchPadUITests`.
final class VirtualPadUITests: XCTestCase {
    /// Touches on the on-screen pad play the game; only touches elsewhere
    /// open the stream menu.
    @MainActor
    func testViewAndMenuShowWithThePad() {
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", "stream", "-virtualPad", "always", "-virtualPadStyle", "system"]
        app.launch()
        XCTAssertTrue(app.buttons["View"].waitForExistence(timeout: 20), "the on-screen pad did not appear")
        XCTAssertTrue(app.buttons["Menu"].exists)
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = "virtual-pad"
        shot.lifetime = .keepAlways
        add(shot)
    }

    @MainActor
    func testPadHiddenWhenSetToNever() {
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", "stream", "-virtualPad", "never", "-virtualPadStyle", "system"]
        app.launch()
        let stats = app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'fps'")).firstMatch
        XCTAssertTrue(stats.waitForExistence(timeout: 20), "the stream did not open")
        XCTAssertFalse(app.buttons["View"].waitForExistence(timeout: 8), "the on-screen pad appeared in Never")
    }

    @MainActor
    func testPadTouchesDoNotOpenTheStreamMenu() {
        XCUIDevice.shared.orientation = .portrait
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", "stream", "-virtualPad", "always", "-virtualPadStyle", "system"]
        app.launch()

        let view = app.buttons["View"]
        XCTAssertTrue(view.waitForExistence(timeout: 20), "the on-screen pad did not appear")
        let menu = app.staticTexts["Input paused"]

        // The A button of the system pad, bottom right in portrait.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.59, dy: 0.92)).tap()
        XCTAssertFalse(menu.waitForExistence(timeout: 2), "a touch on A opened the stream menu")

        view.tap()
        XCTAssertFalse(menu.waitForExistence(timeout: 2), "a touch on View opened the stream menu")

        XCTAssertFalse(app.buttons["streamMenuButton"].exists, "an Xbox stream showed the menu button")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.4)).tap()
        XCTAssertTrue(menu.waitForExistence(timeout: 5), "a touch on the video did not open the stream menu")
    }
}
