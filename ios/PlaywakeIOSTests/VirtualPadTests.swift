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

    @Test func storedModeDefaultsToAuto() throws {
        let defaults = try #require(UserDefaults(suiteName: "VirtualPadTests"))
        defer { defaults.removePersistentDomain(forName: "VirtualPadTests") }
        #expect(VirtualPad.Mode.stored(defaults) == .auto)
        defaults.set("never", forKey: VirtualPad.Mode.key)
        #expect(VirtualPad.Mode.stored(defaults) == .never)
        defaults.set("bogus", forKey: VirtualPad.Mode.key)
        #expect(VirtualPad.Mode.stored(defaults) == .auto)
    }
}
