import QuartzCore
import WebRTC

/// Wraps the default decoders to time the stage after decoding: from the
/// decoder's output callback to the frame reaching the renderer. WebRTC's
/// jitter-buffer and decode stats end before it, but a playout delay can hold
/// decoded frames there until their render time.
final class TimedDecoderFactory: NSObject, RTCVideoDecoderFactory {
    let clock = DecodeClock()
    private let inner = RTCDefaultVideoDecoderFactory()

    func createDecoder(_ info: RTCVideoCodecInfo) -> RTCVideoDecoder? {
        // Only H.264 is wrapped: the other codecs are native builders, which
        // WebRTC cannot use through a wrapper.
        guard info.name == kRTCVideoCodecH264Name else { return inner.createDecoder(info) }
        return inner.createDecoder(info).map { TimedDecoder(inner: $0, clock: clock) }
    }

    func supportedCodecs() -> [RTCVideoCodecInfo] {
        inner.supportedCodecs()
    }
}

/// Frame timing around the decoder: arrival cadence at the decoder input and
/// decode-finish times keyed by RTP timestamp.
final class DecodeClock {
    /// Arrival cadence of frames at the decoder input since the last snapshot.
    struct Arrival {
        var frames = 0
        /// Frames whose gap to the previous frame was over 8 ms longer than
        /// the gap between their RTP timestamps: late on the way to us.
        var late = 0
        var maxLateMs = 0.0
        /// Frames that came under 4 ms after the previous one.
        var bursts = 0
        /// Frames the sender itself stamped over 25 ms after the previous one.
        var senderGaps = 0
        /// Mean of local time minus the frame's estimated capture time.
        var captureAgeMs: Double?
    }

    private let lock = NSLock()
    private var finishedAt: [Int32: CFTimeInterval] = [:]
    private var arrival = Arrival()
    private var lastArrival: (at: CFTimeInterval, rtp: UInt32)?
    private var captureAgeSum = 0.0
    private var captureAgeCount = 0

    /// A frame enters the decoder. `ntpMs` is WebRTC's capture-time estimate
    /// from RTCP sender reports, 0 until the first report.
    func arrived(rtp: UInt32, ntpMs: Int64) {
        let now = CACurrentMediaTime()
        let localNtpMs = (Date().timeIntervalSince1970 + 2_208_988_800) * 1000
        lock.lock()
        defer { lock.unlock() }
        arrival.frames += 1
        if let last = lastArrival {
            let sentMs = Double(rtp &- last.rtp) / 90
            let gotMs = (now - last.at) * 1000
            if sentMs < 1000 {
                let lateMs = gotMs - sentMs
                if lateMs > 8 { arrival.late += 1 }
                arrival.maxLateMs = max(arrival.maxLateMs, lateMs)
                if sentMs > 25 { arrival.senderGaps += 1 }
                if gotMs < 4 { arrival.bursts += 1 }
            }
        }
        lastArrival = (now, rtp)
        let age = localNtpMs - Double(ntpMs)
        if ntpMs > 0, abs(age) < 5000 {
            captureAgeSum += age
            captureAgeCount += 1
        }
    }

    /// Arrival counters since the previous call.
    func snapshotArrival() -> Arrival {
        lock.lock()
        defer { lock.unlock() }
        var result = arrival
        result.captureAgeMs = captureAgeCount > 0 ? captureAgeSum / Double(captureAgeCount) : nil
        arrival = Arrival()
        captureAgeSum = 0
        captureAgeCount = 0
        return result
    }

    func finished(_ timeStamp: Int32) {
        let now = CACurrentMediaTime()
        lock.lock()
        finishedAt[timeStamp] = now
        // Frames the renderer never received (dropped) must not pile up.
        if finishedAt.count > 120 { finishedAt = finishedAt.filter { now - $0.value < 1 } }
        lock.unlock()
    }

    /// Seconds from decode finish to now for this frame; each frame once.
    func held(_ timeStamp: Int32) -> CFTimeInterval? {
        let now = CACurrentMediaTime()
        lock.lock()
        defer { lock.unlock() }
        return finishedAt.removeValue(forKey: timeStamp).map { now - $0 }
    }
}

private final class TimedDecoder: NSObject, RTCVideoDecoder {
    private let inner: RTCVideoDecoder
    private let clock: DecodeClock

    init(inner: RTCVideoDecoder, clock: DecodeClock) {
        self.inner = inner
        self.clock = clock
    }

    func setCallback(_ callback: @escaping RTCVideoDecoderCallback) {
        let clock = self.clock
        inner.setCallback { frame in
            clock.finished(frame.timeStamp)
            callback(frame)
        }
    }

    func startDecode(withNumberOfCores numberOfCores: Int32) -> Int {
        inner.startDecode(withNumberOfCores: numberOfCores)
    }

    func release() -> Int {
        inner.release()
    }

    func decode(_ encodedImage: RTCEncodedImage, missingFrames: Bool,
                codecSpecificInfo info: RTCCodecSpecificInfo?, renderTimeMs: Int64) -> Int {
        clock.arrived(rtp: encodedImage.timeStamp, ntpMs: encodedImage.ntpTimeMs)
        return inner.decode(encodedImage, missingFrames: missingFrames, codecSpecificInfo: info, renderTimeMs: renderTimeMs)
    }

    func implementationName() -> String {
        inner.implementationName()
    }
}
