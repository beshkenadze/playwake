import Foundation
import SwiftUI

final class LabController: ObservableObject {
    @Published private(set) var stats = LabStats()
    @Published private(set) var running = false
    let renderer = LowLatencyVideoRenderer()
    private var rtc: LoopbackRTC?

    func start() {
        guard rtc == nil else { return }
        let session = LoopbackRTC(renderer: renderer)
        session.delegate = self
        rtc = session
        running = true
        session.start()
    }

    func stop() {
        rtc?.stop()
        rtc = nil
        running = false
        stats.state = "stopped"
    }

    deinit {
        rtc?.stop()
    }
}

extension LabController: LoopbackRTCDelegate {
    func loopback(_ rtc: LoopbackRTC, didUpdate stats: LabStats) {
        DispatchQueue.main.async {
            self.stats = stats
            LabLog.write("stats \(stats.summary.replacingOccurrences(of: "\n", with: " | "))")
        }
    }

    func loopback(_ rtc: LoopbackRTC, didChange state: String) {
        DispatchQueue.main.async {
            self.stats.state = state
            if state.hasPrefix("FATAL") || state == "stopped" {
                self.running = false
            }
        }
    }
}
