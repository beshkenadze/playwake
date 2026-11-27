import Testing
@testable import Playwake

struct GamepadPriorityTests {
    @Test func anyPadFillsAnEmptySlot() {
        #expect(Gamepad.prefers(current: nil, new: .touch))
        #expect(Gamepad.prefers(current: nil, new: .physical))
    }

    @Test func onScreenPadDoesNotTakeOverAPhysicalOne() {
        #expect(!Gamepad.prefers(current: .physical, new: .touch))
    }

    @Test func physicalPadTakesOverTheOnScreenOne() {
        #expect(Gamepad.prefers(current: .touch, new: .physical))
    }

    @Test func lastPhysicalPadWins() {
        #expect(Gamepad.prefers(current: .physical, new: .physical))
    }

    @Test func fallbackPrefersAPhysicalPad() {
        #expect(Gamepad.fallback(["touch", "pad"], kind: kind) == "pad")
        #expect(Gamepad.fallback(["touch"], kind: kind) == "touch")
        #expect(Gamepad.fallback([String](), kind: kind) == nil)
    }

    /// Always mode: the physical pad is in use when the on-screen one connects.
    @Test func alwaysModeKeepsThePhysicalPad() {
        var current = adopt(nil, "pad")
        current = adopt(current, "touch")
        #expect(current == "pad")
    }

    /// Auto mode: a physical pad connects while the on-screen one is still
    /// connecting, then the on-screen one is disconnected.
    @Test func hidingTheOnScreenPadDuringAPhysicalConnectKeepsThePhysicalPad() {
        var current = adopt(nil, "pad")
        current = adopt(current, "touch")
        current = disconnect(current, "touch", remaining: ["pad"])
        #expect(current == "pad")
    }

    @Test func losingThePhysicalPadFallsBackToTheOnScreenOne() {
        var current = adopt(nil, "touch")
        current = adopt(current, "pad")
        current = disconnect(current, "pad", remaining: ["touch"])
        #expect(current == "touch")
    }

    @Test func overlayButtonsAreMergedIntoTheSnapshot() {
        let gamepad = Gamepad.shared
        defer { gamepad.overlayButtons = 0 }
        gamepad.overlayButtons = Gamepad.Button.view | Gamepad.Button.menu
        let buttons = gamepad.snapshot().buttons
        #expect(buttons & Gamepad.Button.view != 0)
        #expect(buttons & Gamepad.Button.menu != 0)
    }

    // The same steps as `Gamepad.adopt` and its disconnect observer.
    private func kind(_ name: String) -> Gamepad.Kind { name == "touch" ? .touch : .physical }

    private func adopt(_ current: String?, _ new: String) -> String? {
        Gamepad.prefers(current: current.map(kind), new: kind(new)) ? new : current
    }

    private func disconnect(_ current: String?, _ gone: String, remaining: [String]) -> String? {
        current == gone ? Gamepad.fallback(remaining, kind: kind) : current
    }
}
