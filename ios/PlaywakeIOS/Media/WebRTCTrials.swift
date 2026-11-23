import Foundation
import WebRTC

/// Field trials have to be installed once per process, before any
/// `RTCPeerConnectionFactory` exists.
enum WebRTCTrials {
    private static let installed: Void = {
        RTCInitFieldTrialDictionary([
            // Render every frame as soon as it is decoded instead of honouring
            // the sender's playout-delay hint. On the loopback lab this drops
            // the jitter-buffer component from ~14.8 ms to ~0.2 ms without
            // touching frame rate or drop count.
            "WebRTC-ForcePlayoutDelay": "min_ms:0,max_ms:0",
        ])
        LabLog.write("field trials: ForcePlayoutDelay min_ms:0,max_ms:0")
    }()

    static func install() {
        _ = installed
    }
}
