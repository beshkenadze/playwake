import Testing
import UIKit
@testable import Playwake

struct SoftKeyboardTests {
    private let control = StickyKeys.Modifier.control.rawValue
    private let alt = StickyKeys.Modifier.alt.rawValue
    private let win = StickyKeys.Modifier.win.rawValue
    private let c: UInt16 = 0x06

    @Test func textGoesAsText() {
        var keys = StickyKeys()
        #expect(keys.type("hé") == [.text("hé")])
    }

    @Test func returnIsTheEnterKey() {
        var keys = StickyKeys()
        #expect(keys.type("\n") == [.key(StickyKeys.enter, true), .key(StickyKeys.enter, false)])
    }

    @Test func aLatchedModifierHoldsTheNextKeyAndThenLetsGo() {
        var keys = StickyKeys()
        #expect(keys.tap(.control) == [])
        #expect(keys.type("c") == [.key(control, true), .key(c, true), .key(c, false), .key(control, false)])
        #expect(keys.latched.isEmpty)
        // The next letter is plain text again.
        #expect(keys.type("c") == [.text("c")])
    }

    @Test func modifiersLatchTogetherInTheOrderTapped() {
        var keys = StickyKeys()
        _ = keys.tap(.control)
        _ = keys.tap(.alt)
        #expect(keys.press(StickyKeys.tab) == [.key(control, true), .key(alt, true), .key(StickyKeys.tab, true),
                                               .key(StickyKeys.tab, false), .key(alt, false), .key(control, false)])
    }

    @Test func anUppercaseLetterWithAModifierAddsShift() {
        var keys = StickyKeys()
        _ = keys.tap(.control)
        let shift = StickyKeys.leftShift
        #expect(keys.type("C") == [.key(control, true), .key(shift, true), .key(c, true),
                                   .key(c, false), .key(shift, false), .key(control, false)])
    }

    @Test func aSecondTapSendsTheModifierAlone() {
        var keys = StickyKeys()
        _ = keys.tap(.win)
        #expect(keys.tap(.win) == [.key(win, true), .key(win, false)])
        #expect(keys.latched.isEmpty)
    }

    @Test func textAModifierCannotHoldGoesAsTextAndClearsTheLatch() {
        var keys = StickyKeys()
        _ = keys.tap(.alt)
        #expect(keys.type("é") == [.text("é")])
        #expect(keys.latched.isEmpty)
    }

    @Test func backspaceTakesTheLatchedModifiers() {
        var keys = StickyKeys()
        _ = keys.tap(.control)
        let backspace = StickyKeys.backspace
        #expect(keys.press(backspace) == [.key(control, true), .key(backspace, true),
                                          .key(backspace, false), .key(control, false)])
    }

    @Test func controlAltDeleteDoesNotRepeatALatchedModifier() {
        var keys = StickyKeys()
        _ = keys.tap(.control)
        let delete = StickyKeys.delete
        #expect(keys.chord([control, alt, delete]) == [.key(control, true), .key(alt, true), .key(delete, true),
                                                       .key(delete, false), .key(alt, false), .key(control, false)])
    }

    @Test func lettersDigitsAndSpaceHaveUsages() {
        #expect(StickyKeys.usage(of: "a")! == (0x04, false))
        #expect(StickyKeys.usage(of: "z")! == (0x1D, false))
        #expect(StickyKeys.usage(of: "Z")! == (0x1D, true))
        #expect(StickyKeys.usage(of: "1")! == (0x1E, false))
        #expect(StickyKeys.usage(of: "9")! == (0x26, false))
        #expect(StickyKeys.usage(of: "0")! == (0x27, false))
        #expect(StickyKeys.usage(of: " ")! == (0x2C, false))
        #expect(StickyKeys.usage(of: "ab") == nil)
        #expect(StickyKeys.usage(of: "ж") == nil)
    }

    @Test func aLetterWithACombiningAccentIsNotAShortcut() {
        var keys = StickyKeys()
        _ = keys.tap(.control)
        #expect(StickyKeys.usage(of: "e\u{301}") == nil)
        #expect(keys.type("e\u{301}") == [.text("e\u{301}")])
    }

    @MainActor
    @Test func anUpdateQueuedBeforeANewerOneDoesNotUndoIt() async throws {
        let scene = try #require(UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let window = UIWindow(windowScene: scene)
        let field = KeyCatcherView()
        window.addSubview(field)
        window.makeKeyAndVisible()
        let keyboard = SoftKeyboard.shared
        defer {
            keyboard.setStreamActive(false)
            field.focus()
            window.isHidden = true
        }
        keyboard.setStreamActive(true)
        keyboard.show()
        field.focus()
        #expect(field.isFirstResponder)
        // Hidden and shown again before either update runs.
        keyboard.hide()
        field.scheduleFocus()
        keyboard.show()
        field.scheduleFocus()
        await withCheckedContinuation { done in DispatchQueue.main.async { done.resume() } }
        #expect(keyboard.visible)
        #expect(field.isFirstResponder)
    }

    @MainActor
    @Test func theKeyboardShowsOnlyWhileTheStreamTakesInput() {
        let keyboard = SoftKeyboard.shared
        keyboard.setStreamActive(false)
        // Asked for from the menu: shown once the menu has closed.
        keyboard.show()
        #expect(!keyboard.visible)
        keyboard.setStreamActive(true)
        #expect(keyboard.visible)
        // The menu opens: hidden, and it stays hidden when the menu closes.
        keyboard.tap(.control)
        keyboard.setStreamActive(false)
        #expect(!keyboard.visible)
        #expect(keyboard.keys.latched.isEmpty)
        keyboard.setStreamActive(true)
        #expect(!keyboard.visible)
    }

    @MainActor
    @Test func puttingTheKeyboardAwayForgetsIt() {
        let keyboard = SoftKeyboard.shared
        keyboard.setStreamActive(true)
        keyboard.show()
        #expect(keyboard.visible)
        keyboard.dismissed()
        #expect(!keyboard.visible)
        keyboard.setStreamActive(false)
    }
}
