import Testing
@testable import Playwake

/// Core events through a controller of its own; the app's gets the core's
/// events back afterwards.
@MainActor
@Suite(.serialized)
struct SessionFlowTests {
    private func withController(_ body: (SessionController) throws -> Void) rethrows {
        let core = PlaywakeCore.shared
        let previous = core.onEvent
        defer { core.onEvent = previous }
        try body(SessionController())
    }

    @Test func anUnreadableSavedSignInLeavesTheSplashAndShowsOnlyOnXbox() {
        withController { controller in
            controller.handle(.error("Could not read saved Microsoft sign-in"))
            #expect(controller.account == .signedOut)
            #expect(controller.xboxError == "Could not read saved Microsoft sign-in")
            #expect(controller.screen != .splash)
            #expect(controller.status.isEmpty)
        }
    }

    /// The Mac's Settings command while the saved sign-in is still read.
    @Test(.enabled(if: !PlaywakeCore.shared.hasPairedPc))
    func aSectionAskedForDuringTheSplashOpensHomeAndStays() throws {
        try withController { controller in
            try #require(controller.screen == .splash)
            controller.open(.settings)
            #expect(controller.screen == .home)
            controller.handle(.signedOut)
            #expect(controller.screen == .home)
            #expect(controller.section == .settings)
        }
    }

    @Test func aPcSearchMessageGoesOnceThePcsCome() {
        withController { controller in
            controller.addPc(address: "192.0.2.1")
            #expect(!controller.pcStatus.isEmpty)
            controller.handle(.pcs(computers: [], games: []))
            #expect(controller.pcStatus.isEmpty)
        }
    }
}
