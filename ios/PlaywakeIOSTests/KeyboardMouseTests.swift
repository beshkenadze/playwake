import CoreGraphics
import Foundation
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

    @Test func commandAndTheKeysPressedWithItStayOnThisDevice() {
        let leftCommand: UInt16 = 0xE3, rightCommand: UInt16 = 0xE7, k: UInt16 = 0x0E
        #expect(!KeyboardMouse.forwards(usage: leftCommand, pressed: true, command: true))
        #expect(!KeyboardMouse.forwards(usage: rightCommand, pressed: false, command: false))
        #expect(!KeyboardMouse.forwards(usage: k, pressed: true, command: true))
        #expect(KeyboardMouse.forwards(usage: k, pressed: true, command: false))
    }

    @Test func releasesAlwaysReachTheHost() {
        // K pressed alone, then Command held before K comes up: the release still goes.
        #expect(KeyboardMouse.forwards(usage: 0x0E, pressed: false, command: true))
    }

    @Test func storedModeDefaultsToGame() throws {
        let defaults = try #require(UserDefaults(suiteName: "KeyboardMouseTests"))
        defer { defaults.removePersistentDomain(forName: "KeyboardMouseTests") }
        #expect(KeyboardMouse.Mode.stored(defaults) == .game)
        defaults.set("desktop", forKey: KeyboardMouse.Mode.key)
        #expect(KeyboardMouse.Mode.stored(defaults) == .desktop)
    }
}
