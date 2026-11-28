import Testing
@testable import Playwake

/// Launching without a Microsoft account: the PCs need none.
struct StartTests {
    typealias Controller = SessionController

    @Test func homeOpensOnceSignedInWithAPairedPcOrASectionAskedFor() {
        #expect(Controller.startScreen(account: .signedIn, pairedPc: false, opensSection: false) == .home)
        #expect(Controller.startScreen(account: .restoring, pairedPc: true, opensSection: false) == .home)
        #expect(Controller.startScreen(account: .signedOut, pairedPc: true, opensSection: false) == .home)
        #expect(Controller.startScreen(account: .restoring, pairedPc: false, opensSection: true) == .home)
    }

    @Test func theWelcomeShowsOnlyWithNoAccountAndNoPairedPc() {
        #expect(Controller.startScreen(account: .signedOut, pairedPc: false, opensSection: false) == .welcome)
        #expect(Controller.startScreen(account: .restoring, pairedPc: false, opensSection: false) == .splash)
    }

    @Test func homeWithoutAnAccountOpensOnThePcs() {
        #expect(Controller.startSection(account: .signedOut, asked: nil) == .pcs)
        #expect(Controller.startSection(account: .restoring, asked: nil) == .pcs)
        #expect(Controller.startSection(account: .signedIn, asked: nil) == .consoles)
        #expect(Controller.startSection(account: .signedOut, asked: .settings) == .settings)
    }

    @Test func aSavedSignInThatCannotBeReadLeavesTheAccountSignedOut() {
        let after = Controller.afterError(.restoring, codeShown: false)
        #expect(after.account == .signedOut)
        #expect(after.xbox)
    }

    @Test func aFailedSignInEndsAtOnceOrWithTheCodeWhenItIsDone() {
        #expect(Controller.afterError(.signingIn, codeShown: false) == (.signedOut, true))
        #expect(Controller.afterError(.signingIn, codeShown: true) == (.signingIn, true))
    }

    @Test func anErrorOnceTheAccountIsKnownIsAStatus() {
        #expect(Controller.afterError(.signedIn, codeShown: false) == (.signedIn, false))
        #expect(Controller.afterError(.signedOut, codeShown: false) == (.signedOut, false))
    }

    @Test func anXboxErrorEndsAnXboxStreamButNotAPcStream() {
        #expect(Controller.errorEndsStream(streaming: true, pcStream: false))
        #expect(!Controller.errorEndsStream(streaming: true, pcStream: true))
        #expect(!Controller.errorEndsStream(streaming: false, pcStream: false))
    }

    @Test func settingsSayWhetherMicrosoftIsSignedIn() {
        #expect(Controller.accountSummary(.signedOut, consoles: 0) == "Not signed in")
        #expect(Controller.accountSummary(.restoring, consoles: 0) == "Signing in…")
        #expect(Controller.accountSummary(.signedIn, consoles: 1) == "1 console")
        #expect(Controller.accountSummary(.signedIn, consoles: 0) == "0 consoles")
    }
}
