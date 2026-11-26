import Foundation
import WebRTC

/// Field trials have to be installed once per process, before any
/// `RTCPeerConnectionFactory` exists.
enum WebRTCTrials {
    /// Smoothing when the user has not picked one. Live on an iPhone XS and
    /// 15 Pro, 10 ms cost ~0.3 ms over 0 and cut render stalls by a third;
    /// 50 ms added ~33 ms of latency and still froze for up to ~0.5 s.
    static let defaultPlayoutMs = 10

    /// The Smoothing setting in milliseconds (UserDefaults `playoutMs`).
    static var storedPlayoutMs: Int {
        let defaults = UserDefaults.standard
        return defaults.object(forKey: "playoutMs") == nil
            ? defaultPlayoutMs : defaults.integer(forKey: "playoutMs")
    }

    private static let installed: Void = {
        // Zero delay renders every frame as soon as it is decoded, but exposes
        // bursty network delivery as freezes; a small fixed jitter cushion
        // smooths bursts at the cost of n ms of latency. `-playoutMs <n>`
        // overrides the setting for experiments.
        var playoutMs = storedPlayoutMs
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "-playoutMs"), i + 1 < args.count,
           let n = Int(args[i + 1]), (0...500).contains(n) {
            playoutMs = n
        }
        let value = "min_ms:\(playoutMs),max_ms:\(playoutMs)"
        // The audio jitter buffer targets this quantile of packet arrival
        // delay. WebRTC's 0.95 held ~115 ms of audio under load on Wi-Fi; 0.6
        // holds ~70 ms with under 0.05% concealed samples (iPhone XS, live
        // console). `-netEqQuantile <q>` overrides it for experiments.
        var quantile = 0.6
        if let i = args.firstIndex(of: "-netEqQuantile"), i + 1 < args.count,
           let q = Double(args[i + 1]), (0.5...0.99).contains(q) {
            quantile = q
        }
        let trials = [
            "WebRTC-ForcePlayoutDelay": value,
            "WebRTC-Audio-NetEqDelayManagerConfig": "quantile:\(quantile)",
            // Without the path monitor WebRTC types interfaces by name, so on
            // a Mac every `en*` is Wi-Fi and real Wi-Fi (en0) outranks a wired
            // adapter (en7): the stream ran over Wi-Fi with Ethernet plugged in.
            "WebRTC-Network-UseNWPathMonitor": "Enabled",
        ]
        RTCInitFieldTrialDictionary(trials)
        LabLog.write("field trials: \(trials.sorted { $0.key < $1.key }.map { "\($0.key) \($0.value)" }.joined(separator: "; "))")
    }()

    static func install() {
        _ = installed
    }
}
