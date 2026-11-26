import SwiftUI

@main
struct PlaywakeApp: App {
    @StateObject private var session = SessionController()

    init() {
        // Covers load through URLSession.shared; keep them across scrolls and launches.
        URLCache.shared = URLCache(memoryCapacity: 64 << 20, diskCapacity: 256 << 20)
    }

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
