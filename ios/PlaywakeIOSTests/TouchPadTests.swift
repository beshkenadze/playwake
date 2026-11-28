import Foundation
import Testing
@testable import Playwake

@MainActor
struct TouchPadTests {
    private func defaults(_ name: String) throws -> UserDefaults {
        let defaults = try #require(UserDefaults(suiteName: name))
        defaults.removePersistentDomain(forName: name)
        return defaults
    }

    @Test func theCoresDefaultPadDecodesWithEveryXboxControl() throws {
        let layout = try #require(PadLayout.decode(PadLayout.defaultJSON))
        for portrait in [true, false] {
            let names = Set(layout.elements(portrait: portrait).map(\.name))
            #expect(names == ["Left stick", "Right stick", "D-pad", "A", "B", "X", "Y", "LB", "RB", "LT", "RT",
                              "L3", "R3", "View", "Menu", "Guide"])
        }
        #expect(layout.opacity > 0 && layout.opacity <= 1)
    }

    @Test func keysAndMouseButtonsAreNamedForTheScreen() throws {
        let json = #"{"opacity":1,"portrait":[],"landscape":[{"type":"key","usage":41,"label":"Esc","x":0.5,"y":0.5,"size":0.1},{"type":"mouse","button":"Right","x":0.2,"y":0.5,"size":0.1}]}"#
        let layout = try #require(PadLayout.decode(json))
        #expect(layout.landscape.map(\.name) == ["Esc", "Right click"])
    }

    @Test func playwakesPadIsTheDefaultStyle() throws {
        let defaults = try defaults("TouchPadTests.style")
        #expect(VirtualPad.Style.stored(defaults) == .custom)
        defaults.set("system", forKey: VirtualPad.Style.key)
        #expect(VirtualPad.Style.stored(defaults) == .system)
        defaults.set("bogus", forKey: VirtualPad.Style.key)
        #expect(VirtualPad.Style.stored(defaults) == .custom)
    }

    @Test func aGamesOwnLayoutWinsUntilTheSharedOneIsSavedFromIt() throws {
        let store = PadLayoutStore(defaults: try defaults("TouchPadTests.store"))
        #expect(store.saved(game: "Halo") == nil)
        store.save("shared", game: nil)
        #expect(store.saved(game: "Halo") == "shared")
        store.save("halo", game: "Halo")
        #expect(store.saved(game: "Halo") == "halo")
        #expect(store.saved(game: "Forza") == "shared")
        #expect(store.hasOwn(game: "Halo"))
        store.save("everyone", game: nil, forget: "Halo")
        #expect(!store.hasOwn(game: "Halo"))
        #expect(store.saved(game: "Halo") == "everyone")
    }

    @Test func theMenuOffersTheEditorOnlyWithPlaywakesPad() {
        #expect(SessionController.MenuItem.shown(touches: false, customPad: true)
                == [.resume, .guide, .statistics, .mouse, .controls, .reconnect, .end])
        #expect(!SessionController.MenuItem.shown(touches: true).contains(.controls))
    }

    @Test func editsGoThroughTheCoreAndStayOnTheSurface() throws {
        let json = PadLayout.defaultJSON
        let placed = try #require(PadLayout.decode(PadLayout.take(pw_overlay_place(json, 750, 381, 0, -1, 0.5, 0.2))))
        let unit = PadLayout.unit(CGSize(width: 750, height: 381))
        #expect(unit == 381)
        #expect(abs(placed.landscape[0].x - 0.2 * 381 / 2 / 750) < 1e-6)
        let faint = try #require(PadLayout.decode(PadLayout.take(pw_overlay_opacity(json, 0))))
        #expect(abs(faint.opacity - 0.1) < 1e-6)
    }
}

/// These use the core's one input state, so they run one at a time.
@MainActor
@Suite(.serialized)
struct TouchPadCoreTests {
    @Test func aFingerOnADrawnButtonPressesItInThePad() throws {
        // A new stream's route opens the input gate.
        pw_route_free(pw_route_new())
        _ = PadLayout.take(pw_overlay_set_layout(nil))
        let surface = CGSize(width: 402, height: 778)
        pw_overlay_surface(Float(surface.width), Float(surface.height))
        defer { pw_overlay_surface(0, 0) }
        let layout = try #require(PadLayout.decode(PadLayout.defaultJSON))
        let a = try #require(layout.portrait.first { $0.name == "A" })
        let (x, y) = (Float(a.x * surface.width), Float(a.y * surface.height))
        #expect(pw_overlay_hit(x, y) >= 0)
        #expect(pw_overlay_touch(1, UInt32(PW_INPUT_PHASE_BEGAN), x, y, 0) & UInt32(PW_OVERLAY_HAPTIC) != 0)
        var pad = pw_pad()
        pw_input_pad(&pad, 0)
        #expect(pad.buttons & Gamepad.Button.south != 0)
        pw_overlay_release()
        var released = pw_pad()
        pw_input_pad(&released, 8)
        #expect(released.buttons & Gamepad.Button.south == 0)
    }
}
