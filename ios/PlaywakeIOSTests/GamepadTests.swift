import GameController
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
        let kind: (String) -> Gamepad.Kind = { $0 == "touch" ? .touch : .physical }
        #expect(Gamepad.fallback(["touch", "pad"], kind: kind) == "pad")
        #expect(Gamepad.fallback(["touch"], kind: kind) == "touch")
        #expect(Gamepad.fallback([String](), kind: kind) == nil)
    }
}

/// Drives a real `Gamepad` with snapshot controllers: `physical` holds A,
/// `touch` holds B, so the snapshot tells which one is current.
struct GamepadSelectionTests {
    let physical = Self.pad(holding: \.buttonA)
    let touch = Self.pad(holding: \.buttonB)
    let gamepad: Gamepad

    init() {
        let touch = touch
        gamepad = Gamepad(kind: { $0 === touch ? .touch : .physical })
    }

    /// Always mode: the on-screen pad connects while a physical one is in use.
    @Test func onScreenPadDoesNotTakeInputFromAPhysicalOne() {
        gamepad.adopt(physical)
        gamepad.adopt(touch)
        #expect(gamepad.snapshot().buttons == Gamepad.Button.south)
    }

    /// Auto mode: a physical pad connects while the on-screen one is still
    /// connecting, then the on-screen one is disconnected.
    @Test func hidingTheOnScreenPadKeepsThePhysicalOne() {
        gamepad.adopt(physical)
        gamepad.adopt(touch)
        gamepad.disconnected(touch, remaining: [physical])
        #expect(gamepad.snapshot().buttons == Gamepad.Button.south)
    }

    @Test func physicalPadTakesOverTheOnScreenOne() {
        gamepad.adopt(touch)
        gamepad.adopt(physical)
        #expect(gamepad.snapshot().buttons == Gamepad.Button.south)
    }

    @Test func losingThePhysicalPadFallsBackToTheOnScreenOne() {
        gamepad.adopt(touch)
        gamepad.adopt(physical)
        gamepad.disconnected(physical, remaining: [physical, touch])
        #expect(gamepad.snapshot().buttons == Gamepad.Button.east)
    }

    @Test func padWithoutTheExtendedProfileIsIgnored() {
        gamepad.adopt(touch)
        gamepad.adopt(GCController.withMicroGamepad())
        #expect(gamepad.snapshot().buttons == Gamepad.Button.east)
    }

    @Test func overlayButtonsAreMergedWithThePad() {
        gamepad.adopt(physical)
        gamepad.overlayButtons = Gamepad.Button.view | Gamepad.Button.menu
        #expect(gamepad.snapshot().buttons == Gamepad.Button.south | Gamepad.Button.view | Gamepad.Button.menu)
    }

    @Test func overlayButtonsWorkWithoutAPad() {
        gamepad.overlayButtons = Gamepad.Button.menu
        #expect(gamepad.snapshot().buttons == Gamepad.Button.menu)
    }

    private static func pad(holding button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>) -> GCController {
        let controller = GCController.withExtendedGamepad()
        controller.extendedGamepad?[keyPath: button].setValue(1)
        return controller
    }
}
