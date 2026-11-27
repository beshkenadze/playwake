import CoreGraphics
import Foundation
import Testing
@testable import Playwake

struct TouchInputTests {
    private func place(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x, y: y) }

    @Test func aFingerKeepsItsIdFromLandingToLift() throws {
        var tracker = TouchTracker<Int>()
        let beganCall = tracker.call(for: 7, phase: .began, at: place(0.1, 0.2), direct: false, pressure: 1, now: 10)
        let began = try #require(beganCall)
        let movedCall = tracker.call(for: 7, phase: .moved, at: place(0.3, 0.4), direct: false, pressure: 1, now: 20)
        let moved = try #require(movedCall)
        let endedCall = tracker.call(for: 7, phase: .ended, at: place(0.5, 0.6), direct: false, pressure: 1, now: 30)
        let ended = try #require(endedCall)
        #expect(began.id == moved.id && moved.id == ended.id)
        #expect(ended == TouchCall(id: began.id, phase: .ended, x: 0.5, y: 0.6, pressure: 1, now: 30))
        // Lifted: its id is gone, so a stray move is not sent.
        let call1 = tracker.call(for: 7, phase: .moved, at: place(0.5, 0.6), direct: false, pressure: 1, now: 40)
        #expect(call1 == nil)
    }

    @Test func fingersDownTogetherHaveTheirOwnIds() throws {
        var tracker = TouchTracker<Int>()
        var ids = Set<UInt32>()
        for finger in 1...3 {
            let call = tracker.call(for: finger, phase: .began, at: place(0.5, 0.5), direct: false, pressure: 1, now: 0)
            ids.insert(try #require(call).id)
        }
        #expect(ids.count == 3)
    }

    @Test func aCancelledFingerIsSentAsCancelledAndForgotten() throws {
        var tracker = TouchTracker<Int>()
        _ = tracker.call(for: 1, phase: .began, at: place(0.5, 0.5), direct: false, pressure: 1, now: 0)
        let cancelledCall = tracker.call(for: 1, phase: .cancelled, at: place(0.5, 0.5), direct: false, pressure: 1, now: 5)
        let cancelled = try #require(cancelledCall)
        #expect(cancelled.phase == .cancelled)
        let call2 = tracker.call(for: 1, phase: .ended, at: place(0.5, 0.5), direct: false, pressure: 1, now: 6)
        #expect(call2 == nil)
    }

    @Test func fingersDownBeforeAResetStayOutUntilTheyLift() throws {
        var tracker = TouchTracker<Int>()
        _ = tracker.call(for: 1, phase: .began, at: place(0.5, 0.5), direct: false, pressure: 1, now: 0)
        tracker.reset()
        let call3 = tracker.call(for: 1, phase: .moved, at: place(0.6, 0.5), direct: false, pressure: 1, now: 5)
        #expect(call3 == nil)
        let call4 = tracker.call(for: 1, phase: .ended, at: place(0.6, 0.5), direct: false, pressure: 1, now: 6)
        #expect(call4 == nil)
        let call5 = tracker.call(for: 1, phase: .began, at: place(0.6, 0.5), direct: false, pressure: 1, now: 7)
        #expect(call5 != nil)
    }

    @Test func movesOfAFingerNeverDownAreNotSent() {
        var tracker = TouchTracker<Int>()
        let call6 = tracker.call(for: 1, phase: .moved, at: place(0.5, 0.5), direct: false, pressure: 1, now: 0)
        #expect(call6 == nil)
    }

    @Test func theTrackpadTakesFingersOverTheBarsUnclamped() throws {
        var tracker = TouchTracker<Int>()
        let landed = tracker.call(for: 1, phase: .began, at: place(-0.2, 1.3), direct: false, pressure: 1, now: 0)
        let call = try #require(landed)
        #expect(call.x == -0.2 && call.y == 1.3)
    }

    @Test func directTouchLeavesOutAFingerThatLandsOnTheBars() {
        var tracker = TouchTracker<Int>()
        let call7 = tracker.call(for: 1, phase: .began, at: place(-0.1, 0.5), direct: true, pressure: 1, now: 0)
        #expect(call7 == nil)
        let call8 = tracker.call(for: 1, phase: .ended, at: place(-0.1, 0.5), direct: true, pressure: 1, now: 1)
        #expect(call8 == nil)
    }

    @Test func directTouchStopsAtTheVideoEdge() throws {
        var tracker = TouchTracker<Int>()
        _ = tracker.call(for: 1, phase: .began, at: place(0.9, 0.5), direct: true, pressure: 1, now: 0)
        let movedCall = tracker.call(for: 1, phase: .moved, at: place(1.4, -0.3), direct: true, pressure: 1, now: 1)
        let moved = try #require(movedCall)
        #expect(moved.x == 1 && moved.y == 0)
    }

    @Test func theSurfaceIsTheVideoNotTheBars() {
        // iPhone landscape: wider than 16:9, bars at the sides.
        let landscape = TouchInput.videoSize(in: CGSize(width: 874, height: 402))
        #expect(abs(landscape.width - 402 * 16 / 9) < 0.001 && abs(landscape.height - 402) < 0.001)
        // Portrait: bars above and below.
        let portrait = TouchInput.videoSize(in: CGSize(width: 402, height: 874))
        #expect(abs(portrait.width - 402) < 0.001 && abs(portrait.height - 402 * 9 / 16) < 0.001)
    }

    @Test func phasesAreTheCoreOnes() {
        #expect(TouchPhase.began.core == UInt32(PW_INPUT_PHASE_BEGAN))
        #expect(TouchPhase.moved.core == UInt32(PW_INPUT_PHASE_MOVED))
        #expect(TouchPhase.ended.core == UInt32(PW_INPUT_PHASE_ENDED))
        #expect(TouchPhase.cancelled.core == UInt32(PW_INPUT_PHASE_CANCELLED))
    }

    @Test func storedModeDefaultsToTrackpad() throws {
        let defaults = try #require(UserDefaults(suiteName: "TouchInputTests"))
        defer { defaults.removePersistentDomain(forName: "TouchInputTests") }
        #expect(TouchInput.Mode.stored(defaults) == .trackpad)
        defaults.set("direct", forKey: TouchInput.Mode.key)
        #expect(TouchInput.Mode.stored(defaults) == .direct)
        defaults.set("bogus", forKey: TouchInput.Mode.key)
        #expect(TouchInput.Mode.stored(defaults) == .trackpad)
        #expect(TouchInput.Mode.trackpad.core == UInt32(PW_INPUT_TRACKPAD))
        #expect(TouchInput.Mode.direct.core == UInt32(PW_INPUT_DIRECT))
    }
}
