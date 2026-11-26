import UIKit
import SwiftUI

struct RootView: View {
    @StateObject private var lab = LabController()
    @ObservedObject var session: SessionController

    private var labMode: Bool {
        ProcessInfo.processInfo.arguments.contains("-autolab")
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if labMode {
                LabView(controller: lab)
            } else {
                AppView(controller: session)
            }
        }
        .task {
            #if DEBUG
            let arguments = ProcessInfo.processInfo.arguments
            if arguments.contains("-uiSnapshot") {
                // `-uiSnapshotDelay <s>` waits longer, e.g. for covers to load.
                let asked = arguments.firstIndex(of: "-uiSnapshotDelay")
                    .flatMap { $0 + 1 < arguments.count ? Double(arguments[$0 + 1]) : nil }
                let delay = asked.flatMap { (0...600).contains($0) ? $0 : nil } ?? 2.5
                try? await Task.sleep(for: .seconds(delay))
                Self.snapshotWindow()
            }
            #endif
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

    #if DEBUG
    /// `-uiSnapshot`: draws the key window into Documents/ui-snapshot.png, for
    /// checking screens without screen-recording permission.
    @MainActor
    private static func snapshotWindow() {
        let window = UIApplication.shared.connectedScenes
            .compactMap { ($0 as? UIWindowScene)?.keyWindow }
            .first
        guard let window else { return }
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let url = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("ui-snapshot.png")
        try? image.pngData()?.write(to: url)
        NSLog("ui snapshot written to %@", url.path)
    }
    #endif
}
