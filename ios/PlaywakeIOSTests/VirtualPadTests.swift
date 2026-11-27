import Foundation
import Testing
@testable import Playwake

@MainActor
struct VirtualPadTests {
    @Test func autoShowsOnlyWithoutAPhysicalPad() {
        #expect(VirtualPad.shouldShow(mode: .auto, active: true, physicalPad: false))
        #expect(!VirtualPad.shouldShow(mode: .auto, active: true, physicalPad: true))
    }

    @Test func alwaysShowsEvenWithAPhysicalPad() {
        #expect(VirtualPad.shouldShow(mode: .always, active: true, physicalPad: true))
        #expect(VirtualPad.shouldShow(mode: .always, active: true, physicalPad: false))
    }

    @Test func neverShows() {
        #expect(!VirtualPad.shouldShow(mode: .never, active: true, physicalPad: false))
    }

    @Test func hiddenWhenTheStreamIsNotActive() {
        for mode in [VirtualPad.Mode.auto, .always, .never] {
            #expect(!VirtualPad.shouldShow(mode: mode, active: false, physicalPad: false))
        }
    }

    @Test func storedModeDefaultsToAuto() {
        let defaults = UserDefaults.standard
        let saved = defaults.object(forKey: VirtualPad.Mode.key)
        defer { defaults.set(saved, forKey: VirtualPad.Mode.key) }
        defaults.removeObject(forKey: VirtualPad.Mode.key)
        #expect(VirtualPad.Mode.stored == .auto)
        defaults.set("never", forKey: VirtualPad.Mode.key)
        #expect(VirtualPad.Mode.stored == .never)
    }
}
