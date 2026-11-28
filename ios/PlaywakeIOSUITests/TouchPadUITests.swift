import XCTest

/// Real touches on Playwake's on-screen pad over the stream fixtures.
/// `-inputJournal` shows the pad's presses ("pad A down") and the touches
/// that reached the stream ("touch began"). XCUITest cannot put two fingers
/// on two chosen points, so several fingers at once are the core's and the
/// unit tests' to check.
final class TouchPadUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
        XCUIDevice.shared.orientation = .portrait
    }

    @MainActor
    private func launch(_ fixture: String = "stream", extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", fixture, "-virtualPad", "always", "-virtualPadStyle", "custom",
                               "-touchMode", "trackpad", "-inputJournal"] + extra
        app.launch()
        return app
    }

    @MainActor
    private func element(_ app: XCUIApplication, _ name: String) -> XCUIElement {
        app.buttons["pad.\(name)"]
    }

    @MainActor
    private func journal(_ app: XCUIApplication) -> String {
        let journal = app.staticTexts["inputJournal"]
        return journal.exists ? journal.label : ""
    }

    @MainActor
    private func waitForJournal(_ app: XCUIApplication, toContain text: String, timeout: TimeInterval = 5) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if journal(app).contains(text) { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
        return false
    }

    @MainActor
    private func attach(_ name: String) {
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }

    private let names = ["Left stick", "Right stick", "D-pad", "A", "B", "X", "Y", "LB", "RB", "LT", "RT",
                         "L3", "R3", "View", "Menu", "Guide"]

    @MainActor
    func testTheFullPadShowsUpright() {
        let app = launch()
        XCTAssertTrue(element(app, "A").waitForExistence(timeout: 20), "Playwake's pad did not appear")
        for name in names { XCTAssertTrue(element(app, name).exists, "no \(name) upright") }
        let systemView = app.buttons.matching(NSPredicate(format: "label == 'View' AND identifier != 'pad.View'"))
        XCTAssertEqual(systemView.count, 0, "the system pad's buttons showed too")
        XCTAssertGreaterThan(element(app, "A").frame.midY, app.windows.firstMatch.frame.height * 0.6,
                             "the pad is not below the video")
        attach("touch-pad-portrait")
    }

    @MainActor
    func testTheFullPadShowsSideways() {
        let app = launch(extra: ["-uiLandscape"])
        XCTAssertTrue(element(app, "A").waitForExistence(timeout: 20), "Playwake's pad did not appear")
        let window = app.windows.firstMatch
        let deadline = Date().addingTimeInterval(10)
        while window.frame.width <= window.frame.height, Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.2))
        }
        XCTAssertGreaterThan(window.frame.width, window.frame.height, "the stream did not turn sideways")
        RunLoop.current.run(until: Date().addingTimeInterval(1))
        for name in names { XCTAssertTrue(element(app, name).exists, "no \(name) sideways") }
        XCTAssertGreaterThan(element(app, "A").frame.midX, window.frame.width * 0.75, "A is not on the right")
        attach("touch-pad-landscape")
        // A press works sideways too.
        element(app, "A").tap()
        XCTAssertTrue(waitForJournal(app, toContain: "pad A down"), journal(app))
    }

    @MainActor
    func testATapOnAButtonPressesItAndOpensNoMenu() {
        let app = launch()
        let a = element(app, "A")
        XCTAssertTrue(a.waitForExistence(timeout: 20), "Playwake's pad did not appear")
        a.tap()
        XCTAssertTrue(waitForJournal(app, toContain: "pad A down"), journal(app))
        XCTAssertTrue(waitForJournal(app, toContain: "pad A up"), journal(app))
        let menu = app.staticTexts["Input paused"]
        XCTAssertFalse(menu.waitForExistence(timeout: 2), "a tap on A opened the stream menu")
        // The video is the pad's too: its menu has a button of its own.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45)).tap()
        XCTAssertFalse(menu.waitForExistence(timeout: 2), "a tap on the video opened the menu under the pad")
        app.buttons["streamMenuButton"].tap()
        XCTAssertTrue(menu.waitForExistence(timeout: 5), "the menu button did not open the menu")
        XCTAssertFalse(a.exists, "the pad stayed over the menu")
        app.buttons["On-screen controller"].tap()
        XCTAssertTrue(app.switches["Only for Living Room"].waitForExistence(timeout: 5),
                      "the editor from a stream offers no layout of the game's own")
        app.buttons["Cancel"].tap()
        XCTAssertTrue(menu.waitForExistence(timeout: 5), "the menu did not stay open under the editor")
    }

    @MainActor
    func testAStickFollowsADragAndLetsGo() {
        let app = launch()
        let stick = element(app, "Left stick")
        XCTAssertTrue(stick.waitForExistence(timeout: 20), "Playwake's pad did not appear")
        let center = stick.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        center.press(forDuration: 0.3, thenDragTo: center.withOffset(CGVector(dx: 50, dy: -20)))
        XCTAssertTrue(waitForJournal(app, toContain: "pad Left stick down"), journal(app))
        XCTAssertTrue(waitForJournal(app, toContain: "pad Left stick up"), journal(app))
    }

    @MainActor
    func testPadTouchesStayOutOfAPcStream() {
        let app = launch("pcstream")
        let a = element(app, "A")
        XCTAssertTrue(a.waitForExistence(timeout: 20), "Playwake's pad did not appear")
        a.tap()
        element(app, "Guide").tap()
        XCTAssertTrue(waitForJournal(app, toContain: "pad Guide up"), journal(app))
        XCTAssertFalse(journal(app).contains("touch"), "a pad touch reached the stream: \(journal(app))")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45)).tap()
        XCTAssertTrue(waitForJournal(app, toContain: "touch began"), "a touch beside the pad did not reach the stream")
    }

    @MainActor
    func testTheEditorMovesAControlAndKeepsIt() {
        let app = launch("settings")
        let edit = app.buttons["Edit on-screen controller"]
        for _ in 0..<4 where !edit.exists { app.swipeUp() }
        XCTAssertTrue(edit.waitForExistence(timeout: 10), "no editor in Settings")
        edit.tap()
        let a = element(app, "A")
        XCTAssertTrue(a.waitForExistence(timeout: 5), "the editor did not open")
        app.buttons["Reset"].tap()
        app.buttons["Save"].tap()
        edit.tap()
        XCTAssertTrue(a.waitForExistence(timeout: 5))
        let before = a.frame
        let center = a.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        center.press(forDuration: 0.2, thenDragTo: center.withOffset(CGVector(dx: -60, dy: -40)))
        attach("touch-pad-editor")
        app.buttons["Save"].tap()
        edit.tap()
        XCTAssertTrue(a.waitForExistence(timeout: 5))
        let after = a.frame
        XCTAssertEqual(after.midX - before.midX, -60, accuracy: 6, "\(before) -> \(after)")
        XCTAssertEqual(after.midY - before.midY, -40, accuracy: 6, "\(before) -> \(after)")
        app.buttons["Reset"].tap()
        app.buttons["Save"].tap()
    }
}
