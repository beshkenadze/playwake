import SwiftUI

struct RootView: View {
    @StateObject private var lab = LabController()
    @StateObject private var session = SessionController()

    private var labMode: Bool {
        ProcessInfo.processInfo.arguments.contains("-autolab")
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if labMode {
                LabView(controller: lab)
            } else {
                SessionView(controller: session)
            }
        }
        .task {
            let core = PlaywakeCore.shared
            let handshake = core.handshake(width: 1280, height: 720)
            NSLog("playwake-core %@ handshake=%d open=%d",
                  PlaywakeCore.version, handshake?.messages.count ?? -1, handshake?.open.count ?? -1)
            if !labMode && ProcessInfo.processInfo.arguments.contains("-autosignin") {
                try? await Task.sleep(nanoseconds: 800_000_000)
                session.signIn()
            }
            guard labMode else { return }
            try? await Task.sleep(nanoseconds: 1_200_000_000)
            lab.start()
        }
    }
}
