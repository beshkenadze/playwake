import XCTest

/// Real touches on the stream fixtures: a PC's takes them, an Xbox's opens
/// its menu. `-inputJournal` shows the touch and keyboard calls the app made
/// to the core.
final class TouchInputUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
        XCUIDevice.shared.orientation = .portrait
    }

    @MainActor
    private func launch(touch: String = "trackpad", pad: String = "never", extra: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", "pcstream", "-virtualPad", pad, "-touchMode", touch, "-inputJournal"] + extra
        app.launch()
        XCTAssertTrue(app.otherElements["touchSurface"].waitForExistence(timeout: 20), "the stream did not take touches")
        return app
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

    @MainActor
    func testATapOnTheVideoGoesToTheStreamAndLeavesTheMenuShut() {
        let app = launch()
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(waitForJournal(app, toContain: "touch began"), journal(app))
        XCTAssertTrue(waitForJournal(app, toContain: "touch ended"), journal(app))
        XCTAssertFalse(app.staticTexts["Input paused"].waitForExistence(timeout: 2), "a tap on the video opened the menu")
    }

    @MainActor
    func testTheMenuButtonOpensTheMenuWithTheTouchSettings() {
        let app = launch()
        app.buttons["streamMenuButton"].tap()
        XCTAssertTrue(app.staticTexts["Input paused"].waitForExistence(timeout: 5), "the menu button did not open the menu")
        let touch = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Touch'")).firstMatch
        XCTAssertTrue(touch.exists)
        XCTAssertTrue(touch.label.contains("Trackpad"), touch.label)
        XCTAssertTrue(app.buttons["Keyboard"].exists)
        attach("menu-with-touch")
        touch.tap()
        let direct = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Touch' AND label CONTAINS 'Direct'")).firstMatch
        XCTAssertTrue(direct.waitForExistence(timeout: 2), "the touch mode did not change")
        // A tap beside the open menu closes it.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).tap()
        XCTAssertTrue(app.otherElements["touchSurface"].waitForExistence(timeout: 5), "a tap beside the menu did not close it")
        XCTAssertFalse(journal(app).contains("touch"), "the menu button touched the stream: \(journal(app))")
    }

    @MainActor
    func testTouchesOnTheOnScreenPadStayOutOfTheStream() {
        let app = launch(pad: "always")
        let view = app.buttons["View"]
        XCTAssertTrue(view.waitForExistence(timeout: 20), "the on-screen pad did not appear")
        // The A button of the system pad, bottom right in portrait.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.59, dy: 0.92)).tap()
        view.tap()
        XCTAssertFalse(waitForJournal(app, toContain: "touch", timeout: 2), "a pad touch reached the stream: \(journal(app))")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45)).tap()
        XCTAssertTrue(waitForJournal(app, toContain: "touch began"), "a touch beside the pad did not reach the stream")
    }

    @MainActor
    func testThreeFingersShowTheKeyBar() {
        let app = launch()
        app.otherElements["touchSurface"].tap(withNumberOfTaps: 1, numberOfTouches: 3)
        let escape = app.buttons["Esc"]
        XCTAssertTrue(escape.waitForExistence(timeout: 5), "three fingers did not show the keyboard: \(journal(app))")
        attach("key-bar")
        escape.tap()
        XCTAssertTrue(waitForJournal(app, toContain: "key 0x29 down"), journal(app))
        // Alt latches until F4: Alt+F4.
        app.buttons["Alt"].tap()
        app.buttons["F4"].tap()
        XCTAssertTrue(waitForJournal(app, toContain: "key 0xE2 down\nkey 0x3D down\nkey 0x3D up\nkey 0xE2 up"), journal(app))
        app.buttons["Ctrl+Alt+Del"].tap()
        XCTAssertTrue(waitForJournal(app, toContain: "key 0xE0 down\nkey 0xE2 down\nkey 0x4C down\nkey 0x4C up\nkey 0xE2 up\nkey 0xE0 up"),
                      journal(app))
        app.buttons["Hide keyboard"].tap()
        XCTAssertTrue(escape.waitForNonExistence(timeout: 5), "the keyboard did not hide")
    }

    @MainActor
    func testTheKeyboardTypesTextAndKeys() throws {
        let app = launch()
        app.buttons["streamMenuButton"].tap()
        app.buttons["Keyboard"].tap()
        XCTAssertTrue(app.buttons["Esc"].waitForExistence(timeout: 5), "the menu did not show the keyboard")
        XCTAssertFalse(app.staticTexts["Input paused"].exists, "the menu stayed open")
        guard app.keyboards.firstMatch.waitForExistence(timeout: 3) else {
            throw XCTSkip("the simulator types with its hardware keyboard; the key bar was checked")
        }
        attach("keyboard")
        app.typeText("hi")
        XCTAssertTrue(waitForJournal(app, toContain: "text h\ntext i"), journal(app))
        app.typeText("\n")
        XCTAssertTrue(waitForJournal(app, toContain: "key 0x28 down"), journal(app))
        app.keyboards.keys["delete"].tap()
        XCTAssertTrue(waitForJournal(app, toContain: "key 0x2A down"), journal(app))
        app.buttons["Ctrl"].tap()
        app.typeText("c")
        XCTAssertTrue(waitForJournal(app, toContain: "key 0xE0 down\nkey 0x06 down"), journal(app))
    }

    @MainActor
    func testDirectTouchLandsWhereTheFingerIs() {
        let app = launch(touch: "direct")
        // Portrait: the 16:9 video sits in the middle, bars above and below.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.15)).tap()
        // The surface's frame is the video: a quarter across, three quarters down.
        app.otherElements["touchSurface"].coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.75)).tap()
        XCTAssertTrue(waitForJournal(app, toContain: "touch began 0.25 0.75"), journal(app))
        XCTAssertTrue(waitForJournal(app, toContain: "touch ended 0.25 0.75"), journal(app))
        XCTAssertEqual(journal(app).components(separatedBy: "touch began").count - 1, 1,
                       "a touch on the bars reached the stream: \(journal(app))")
    }

    /// The view that follows a mouse in desktop mode lets fingers through.
    @MainActor
    func testFingersPassThroughThePointerArea() {
        let app = launch(extra: ["-pointerArea"])
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(waitForJournal(app, toContain: "touch began"), "the pointer area took the finger: \(journal(app))")
        XCTAssertTrue(waitForJournal(app, toContain: "touch ended"), journal(app))
    }

    @MainActor
    func testAnXboxStreamOpensItsMenuWithATapAndHasNoMenuButton() {
        let app = XCUIApplication()
        app.launchArguments = ["-uiFixture", "stream", "-virtualPad", "never", "-inputJournal"]
        app.launch()
        let stats = app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'fps'")).firstMatch
        XCTAssertTrue(stats.waitForExistence(timeout: 20), "the stream did not open")
        XCTAssertFalse(app.otherElements["touchSurface"].exists, "an Xbox stream took touches")
        XCTAssertFalse(app.buttons["streamMenuButton"].exists, "an Xbox stream showed the menu button")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertTrue(app.staticTexts["Input paused"].waitForExistence(timeout: 5), "a tap on the video did not open the menu")
        XCTAssertFalse(app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Touch'")).firstMatch.exists,
                       "an Xbox menu has the touch row")
        XCTAssertFalse(app.buttons["Keyboard"].exists, "an Xbox menu has the keyboard row")
        XCTAssertFalse(journal(app).contains("touch"), "a tap reached the stream: \(journal(app))")
    }
}
