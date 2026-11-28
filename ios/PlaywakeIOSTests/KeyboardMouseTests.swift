import CoreGraphics
import Foundation
import GameController
import Testing
@testable import Playwake

struct KeyboardMouseTests {
    private let wide: CGFloat = 16.0 / 9.0

    @Test func aVideoThatFillsTheViewMapsCornerToCorner() throws {
        let size = CGSize(width: 1600, height: 900)
        #expect(KeyboardMouse.videoPosition(of: .zero, in: size, aspect: wide) == .zero)
        #expect(KeyboardMouse.videoPosition(of: CGPoint(x: 1600, y: 900), in: size, aspect: wide) == CGPoint(x: 1, y: 1))
    }

    @Test func barsAboveAndBelowAreLeftOutOfAWindowTallerThanTheVideo() throws {
        // 1600×900 of video centred in 1600×1100: 100-point bars top and bottom.
        let size = CGSize(width: 1600, height: 1100)
        let top = try #require(KeyboardMouse.videoPosition(of: CGPoint(x: 800, y: 100), in: size, aspect: wide))
        #expect(top == CGPoint(x: 0.5, y: 0))
        let bar = try #require(KeyboardMouse.videoPosition(of: CGPoint(x: 0, y: 50), in: size, aspect: wide))
        #expect(bar.y < 0)
    }

    @Test func barsLeftAndRightAreLeftOutOfAWindowWiderThanTheVideo() throws {
        // 1600×900 of video centred in 2000×900: 200-point bars at the sides.
        let size = CGSize(width: 2000, height: 900)
        let point = try #require(KeyboardMouse.videoPosition(of: CGPoint(x: 1800, y: 450), in: size, aspect: wide))
        #expect(point == CGPoint(x: 1, y: 0.5))
    }

    @Test func anEmptyViewHasNoPosition() {
        #expect(KeyboardMouse.videoPosition(of: .zero, in: .zero, aspect: wide) == nil)
    }

    private func keys(_ command: inout CommandKey, _ usage: UInt16, _ pressed: Bool) -> [String] {
        command.keys(usage: usage, pressed: pressed).map { "\(String($0.usage, radix: 16))\($0.pressed ? "+" : "-")" }
    }

    @Test func commandAloneTapsTheWindowsKey() {
        var command = CommandKey()
        #expect(keys(&command, 0xE3, true) == [])
        #expect(keys(&command, 0xE3, false) == ["e3+", "e3-"])
    }

    @Test func commandWithAKeyIsWindowsWithThatKey() {
        // Cmd+D: Win+D shows the PC's desktop; no Start tap after it.
        var command = CommandKey()
        #expect(keys(&command, 0xE3, true) == [])
        #expect(keys(&command, 0x07, true) == ["e3+", "7+"])
        #expect(keys(&command, 0x07, false) == ["7-"])
        #expect(keys(&command, 0x08, true) == ["8+"])
        #expect(keys(&command, 0x08, false) == ["8-"])
        #expect(keys(&command, 0xE3, false) == ["e3-"])
    }

    @Test func theMacsShortcutsStayOnTheMacWithNoStartTap() {
        // Cmd+K opens the stream menu, Cmd+Tab switches apps: nothing reaches the PC.
        for key: UInt16 in [0x0E, 0x2B] {
            var command = CommandKey()
            #expect(keys(&command, 0xE7, true) == [])
            #expect(keys(&command, key, true) == [])
            #expect(keys(&command, key, false) == ["\(String(key, radix: 16))-"])
            #expect(keys(&command, 0xE7, false) == [])
        }
    }

    @Test func bothCommandKeysMakeOnePress() {
        // Released together, each key may see the other already up: still one tap.
        var command = CommandKey()
        #expect(keys(&command, 0xE3, true) == [])
        #expect(keys(&command, 0xE7, true) == [])
        #expect(keys(&command, 0xE3, false) == [])
        #expect(keys(&command, 0xE7, false) == ["e3+", "e3-"])
        // And with a key: Win comes up once, after the last Command key.
        #expect(keys(&command, 0xE3, true) == [])
        #expect(keys(&command, 0x07, true) == ["e3+", "7+"])
        #expect(keys(&command, 0xE7, true) == [])
        #expect(keys(&command, 0xE3, false) == [])
        #expect(keys(&command, 0xE7, false) == ["e3-"])
    }

    @Test func aCommandPressedBeforeTheInputOpenedIsNoWinTap() {
        // Command went down while the input was closed (menu open, another app):
        // it counts for nothing, keys typed with it go plain and its release taps nothing.
        let gate = InputGate(), command = CommandKey.Box()
        var sent: [String] = []
        func change(_ usage: UInt16, _ pressed: Bool) {
            command.keyChanged(usage, pressed, command: false, gate: gate) { sent.append("\(String($0, radix: 16))\($1 ? "+" : "-")") }
        }
        change(0xE3, true)
        gate.set(open: true, desktop: false) { _, _ in }
        change(0x07, true)
        change(0x07, false)
        change(0xE3, false)
        #expect(sent == ["7+", "7-"])
    }

    @Test func closingTheInputForgetsTheCommandPress() {
        // Cmd+D, the input closes (the host lets go of Win) and opens again while
        // Command is still down: its release sends nothing more.
        let gate = InputGate(), command = CommandKey.Box()
        var sent: [String] = []
        func change(_ usage: UInt16, _ pressed: Bool) {
            command.keyChanged(usage, pressed, command: false, gate: gate) { sent.append("\(String($0, radix: 16))\($1 ? "+" : "-")") }
        }
        gate.set(open: true, desktop: false) { _, _ in }
        change(0xE3, true)
        change(0x07, true)
        gate.set(open: false, desktop: false) { closing, _ in if closing { KeyboardMouse.inputClosed(command) } }
        gate.set(open: true, desktop: false) { _, _ in }
        change(0x07, false)
        change(0xE3, false)
        #expect(sent == ["e3+", "7+", "7-"])
    }

    @Test func theMacsShortcutsStayOnTheMacWithACommandTheStateMissed() {
        // Command held through Resume (the gate was closed when it went down): Cmd+K
        // still opens the menu here and sends no K; other keys go plain, with no Win.
        var command = CommandKey()
        #expect(command.keys(usage: 0x0E, pressed: true, command: true).isEmpty)
        #expect(command.keys(usage: 0x07, pressed: true, command: true).map(\.usage) == [0x07])
    }

    @Test func keysWithoutCommandAndAllReleasesGoAsTheyAre() {
        var command = CommandKey()
        #expect(keys(&command, 0x0E, true) == ["e+"])
        // K pressed alone, then Command held before K comes up: the release still goes.
        #expect(keys(&command, 0xE3, true) == [])
        #expect(keys(&command, 0x0E, false) == ["e-"])
    }

    @Test func theLeftButtonFollowsThePositionAndOnlyWhenItChanges() {
        let place = CGPoint(x: 0.25, y: 0.5)
        #expect(KeyboardMouse.pointerEvents(at: place, pressed: true, held: false) == [.position(place), .left(true)])
        #expect(KeyboardMouse.pointerEvents(at: place, pressed: true, held: true) == [.position(place)])
        #expect(KeyboardMouse.pointerEvents(at: place, pressed: nil, held: true) == [.position(place)])
        #expect(KeyboardMouse.pointerEvents(at: place, pressed: false, held: true) == [.position(place), .left(false)])
        // Off the video or with no view left, the release still goes.
        #expect(KeyboardMouse.pointerEvents(at: nil, pressed: false, held: true) == [.left(false)])
        #expect(KeyboardMouse.pointerEvents(at: nil, pressed: true, held: false) == [])
    }

    @Test func aClosingGateWaitsForTheSendInProgress() {
        #expect(sendThenSet(open: false, desktop: false) == ["send", "release"])
    }

    @Test func aModeChangeWaitsForTheSendInProgress() {
        #expect(sendThenSet(open: true, desktop: true) == ["send", "mode"])
    }

    /// A send that is in progress when the gate changes: the change waits for it.
    private func sendThenSet(open: Bool, desktop: Bool) -> [String] {
        let gate = InputGate()
        gate.set(open: true, desktop: false) { _, _ in }
        let order = OrderLog()
        let sending = DispatchSemaphore(value: 0), closing = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            gate.send {
                sending.signal()
                _ = closing.wait(timeout: .now() + 1)
                usleep(50_000)
                order.append("send")
            }
        }
        sending.wait()
        closing.signal()
        gate.set(open: open, desktop: desktop) { closed, modeChanged in
            if closed { order.append("release") }
            if modeChanged { order.append("mode") }
        }
        gate.send(inDesktop: false) { order.append("late") }
        return order.entries
    }

    @Test func desktopModeLeavesTheLeftButtonToTheView() {
        let gate = InputGate()
        gate.set(open: true, desktop: true) { _, _ in }
        var sent = 0
        gate.send(inDesktop: false) { sent += 1 }
        gate.send { sent += 10 }
        #expect(sent == 10)
    }

    @Test func aTapOpensTheMenuUnlessThePcOrTheCapturedMouseTakesIt() {
        #expect(KeyboardMouse.tapOpensMenu(pcStream: true, mouseConnected: false, pointerLocked: false))
        #expect(!KeyboardMouse.tapOpensMenu(pcStream: true, mouseConnected: true, pointerLocked: false))
        #expect(KeyboardMouse.tapOpensMenu(pcStream: false, mouseConnected: false, pointerLocked: false))
        #expect(KeyboardMouse.tapOpensMenu(pcStream: false, mouseConnected: true, pointerLocked: false))
        #expect(!KeyboardMouse.tapOpensMenu(pcStream: false, mouseConnected: true, pointerLocked: true))
    }

    @Test func storedModeDefaultsToGame() throws {
        let defaults = try #require(UserDefaults(suiteName: "KeyboardMouseTests"))
        defer { defaults.removePersistentDomain(forName: "KeyboardMouseTests") }
        #expect(KeyboardMouse.Mode.stored(defaults) == .game)
        defaults.set("desktop", forKey: KeyboardMouse.Mode.key)
        #expect(KeyboardMouse.Mode.stored(defaults) == .desktop)
    }
}

/// These read the core's one input state through the Xbox pad, which a key
/// drives while no PC streams, so they run one at a time.
@MainActor
@Suite(.serialized)
struct KeyboardMouseDisconnectTests {
    private let w: UInt16 = 0x1A

    private func stickY(at now: Double) -> Int16 {
        var pad = pw_pad()
        pw_input_pad(&pad, now)
        return pad.axes.1
    }

    @Test func aKeyboardThatGoesAwayLetsGoOfItsKeys() async throws {
        _ = KeyboardMouse.shared
        pw_input_key(KeyboardMouse.keyboard, KeyboardMouse.keyboardPage, w, true)
        #expect(stickY(at: 0) != 0)
        NotificationCenter.default.post(name: .GCKeyboardDidDisconnect, object: nil)
        try await Task.sleep(for: .milliseconds(100))
        #expect(stickY(at: 1) == 0)
    }

    @Test func theReleaseComesAfterTheDevicesQueuedEvents() async throws {
        let keyboardMouse = KeyboardMouse.shared
        let queued = DispatchSemaphore(value: 0)
        let pressed = OrderLog()
        let w = w
        keyboardMouse.queue.async {
            queued.wait()
            pw_input_key(KeyboardMouse.keyboard, KeyboardMouse.keyboardPage, w, true)
            pressed.append("W")
        }
        NotificationCenter.default.post(name: .GCKeyboardDidDisconnect, object: nil)
        queued.signal()
        try await Task.sleep(for: .milliseconds(100))
        keyboardMouse.queue.sync {}
        #expect(pressed.entries == ["W"])
        #expect(stickY(at: 2) == 0)
    }
}

private final class OrderLog: @unchecked Sendable {
    private let lock = NSLock()
    private var log: [String] = []

    var entries: [String] { lock.withLock { log } }

    func append(_ entry: String) { lock.withLock { log.append(entry) } }
}
