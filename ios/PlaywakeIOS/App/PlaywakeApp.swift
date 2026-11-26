import SwiftUI

@main
struct PlaywakeApp: App {
    @StateObject private var session = SessionController()

    var body: some Scene {
        WindowGroup {
            RootView(session: session)
                .preferredColorScheme(.dark)
        }
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { session.section = .settings }
                    .keyboardShortcut(",", modifiers: .command)
            }
        }
    }
}
