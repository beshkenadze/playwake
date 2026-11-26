import CoreVideo
import Foundation
import QuartzCore

/// Input-to-motion latency trials, ported from `media-worker/src/trial.rs`.
///
/// Each attempt holds a 300 ms baseline, pulses the right stick to +0.9 for
/// 230 ms, and finds the camera turn by the horizontal shift of a sparse luma
/// grid. The sample is the time from polling the first deflected input packet
/// to the arrival of the first moved frame at the renderer (the desktop sink
/// point), confirmed by a second moved frame within 85 ms. Display time is not
/// included; the renderer reports it separately as `pres`.
final class LatencyTrials {
    static let rows = 110
    static let cols = 320

    private enum Phase { case idle, baseline, measuring }

    /// Timers and frame analysis share one serial queue, so actuation and
    /// timeout always run after the frames that arrived before them.
    private let queue = DispatchQueue(label: "io.playwake.trials", qos: .userInitiated)
    private let lock = NSLock()

    /// Called when a pulse starts, like a controller event; set before `run`.
    var onActuate: (() -> Void)?

    // Attempt state. `epoch` changes with every attempt so that frames queued
    // for analysis never leak into the next one.
    private var epoch = 0
    private var phase = Phase.idle
    private var previous: [Int16]?
    private var frames = 0
    private var baselineMoved = false
    private var candidate: Double?
    private var pulse: (value: Int16, until: Double)?
    private var awaitingSend = false
    private var actuatedAt = 0.0
    private var polledAt: Double?
    private var sentAt: Double?

    // Stream steadiness: frames keep coming at one size.
    private var steadySince: Double?
    private var lastFrameAt: Double?
    private var frameSize = (0, 0)

    // Run state.
    private var running = false
    private var stopped = false
    private var runStartedAt = 0.0
    private var expectedHeight = 0
    private var attempts = 0
    private var turns: [Double] = []
    private var sends: [Double] = []
    private var waits: [Double] = []
    private var unstable = 0
    private var missed = 0
    private var context = ""
    private var loggedCost = false

    /// Latest stream stats line for the summary.
    func updateContext(_ line: String) {
        lock.lock(); context = line; lock.unlock()
    }

    // MARK: - Schedule (on `queue`)

    /// Starts a run on frames of `height` (the requested stream height).
    func run(height: Int) {
        lock.lock()
        let first = !running && !stopped
        if first {
            running = true
            runStartedAt = Self.nowMs()
            expectedHeight = height
            attempts = 0
            turns = []
            sends = []
            waits = []
            unstable = 0
            missed = 0
        }
        lock.unlock()
        guard first else { return }
        LabLog.write("trial self-check \(Self.selfCheck() ? "ok" : "FAILED")")
        queue.async { self.attempt() }
    }

    /// Ends the current run for good; later `run` calls are ignored.
    func cancel() {
        lock.lock()
        stopped = true
        running = false
        epoch += 1
        phase = .idle
        pulse = nil
        lock.unlock()
    }

    private func attempt() {
        lock.lock()
        guard running else { lock.unlock(); return }
        if turns.count >= 8 || attempts >= 12 {
            lock.unlock()
            summary()
            lock.lock(); running = false; lock.unlock()
            return
        }
        let now = Self.nowMs()
        let fresh = lastFrameAt.map { now - $0 < 100 } ?? false
        let sized = abs(frameSize.1 - expectedHeight) <= 16
        let steady = fresh && sized && (steadySince.map { now - $0 >= 4_000 } ?? false)
        guard steady else {
            if now - runStartedAt > 30_000 {
                let (size, started) = (frameSize, attempts > 0)
                lock.unlock()
                LabLog.write("trials stopped: no steady \(expectedHeight)p video for 30 s (last frame \(size.0)x\(size.1))")
                // Report what was measured before the video went away; the run
                // is released only after, so a new run cannot reset the samples.
                if started { summary() } else { LabLog.write("trials done: no samples (no steady video)") }
                lock.lock(); running = false; lock.unlock()
                return
            }
            lock.unlock()
            queue.asyncAfter(deadline: .now() + 0.5) { self.attempt() }
            return
        }
        attempts += 1
        runStartedAt = now
        epoch += 1
        phase = .baseline
        previous = nil
        frames = 0
        baselineMoved = false
        candidate = nil
        polledAt = nil
        sentAt = nil
        let current = epoch
        lock.unlock()

        // A strict timer with a random offset models a stick event at any point
        // of the input tick; a coalescing timer would fire with the tick.
        let actuation = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        actuation.schedule(deadline: .now() + 0.3 + .random(in: 0..<0.02), leeway: .nanoseconds(0))
        actuation.setEventHandler { [weak self] in
            self?.actuate(current)
            actuation.cancel()
        }
        actuation.resume()
        queue.asyncAfter(deadline: .now() + 1.5) { self.expire(current) }
        queue.asyncAfter(deadline: .now() + 1.8) { self.attempt() }
    }

    private func actuate(_ current: Int) {
        lock.lock()
        guard epoch == current, phase == .baseline else { lock.unlock(); return }
        if baselineMoved || frames < 4 {
            phase = .idle
            unstable += 1
            let attempt = attempts
            lock.unlock()
            LabLog.write("trial \(attempt)/12 rejected: unstable scene")
            return
        }
        phase = .measuring
        actuatedAt = Self.nowMs()
        pulse = (Int16(0.9 * 32767), actuatedAt + 230)
        awaitingSend = true
        lock.unlock()
        onActuate?()
        queue.asyncAfter(deadline: .now() + 0.55) {
            self.lock.lock()
            let restore = self.epoch == current
            if restore {
                self.awaitingSend = false
                self.pulse = (Int16(-0.9 * 32767), Self.nowMs() + 230)
            }
            self.lock.unlock()
            if restore { self.onActuate?() }
        }
    }

    private func expire(_ current: Int) {
        lock.lock()
        guard epoch == current, phase == .measuring else { lock.unlock(); return }
        phase = .idle
        missed += 1
        let attempt = attempts
        lock.unlock()
        LabLog.write("trial \(attempt)/12 rejected: no confirmed camera turn")
    }

    private func summary() {
        lock.lock()
        let turns = self.turns.sorted()
        let sends = self.sends.sorted()
        let waits = self.waits.sorted()
        let (unstable, missed, context) = (self.unstable, self.missed, self.context)
        lock.unlock()
        guard !turns.isEmpty else {
            LabLog.write("trials done: no samples (unstable \(unstable), no turn \(missed)) | \(context)")
            return
        }
        func pct(_ values: [Double], _ p: Double) -> Double {
            values[min(values.count - 1, Int(Double(values.count - 1) * p + 0.5))]
        }
        LabLog.write(String(
            format: "trials done: n=%d poll→turn median %.1f p90 %.1f min %.1f max %.1f ms, poll→send median %.2f ms, actuate→send median %.2f ms (unstable %d, no turn %d) | %@",
            turns.count, pct(turns, 0.5), pct(turns, 0.9), turns[0], turns[turns.count - 1],
            pct(sends, 0.5), waits.isEmpty ? 0 : pct(waits, 0.5), unstable, missed, context))
    }

    // MARK: - Input (input queue)

    /// Right stick X override while a pulse is active.
    func stickX(at now: Double) -> Int16? {
        lock.lock(); defer { lock.unlock() }
        guard let pulse else { return nil }
        if now < pulse.until { return pulse.value }
        self.pulse = nil
        return nil
    }

    /// Records the first successfully sent packet of the forward pulse.
    func sent(polledAt polled: Double, sentAt sent: Double) {
        lock.lock(); defer { lock.unlock() }
        guard awaitingSend else { return }
        awaitingSend = false
        polledAt = polled
        sentAt = sent
        waits.append(sent - actuatedAt)
    }

    // MARK: - Frames (renderer callback)

    /// `arrival` is the renderer callback time in `CACurrentMediaTime` ms.
    /// Copies the sample grid here so the decoder buffer is released at once.
    func observe(_ buffer: CVPixelBuffer, arrival: Double) {
        let size = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer))
        lock.lock()
        if let last = lastFrameAt, arrival - last > 100 { steadySince = nil }
        if size != frameSize { steadySince = nil; frameSize = size }
        if steadySince == nil { steadySince = arrival }
        lastFrameAt = arrival
        let (current, phase) = (epoch, self.phase)
        lock.unlock()
        guard phase != .idle else { return }
        let started = Self.nowMs()
        guard let sample = Self.sample(buffer) else { return }
        let cost = Self.nowMs() - started
        queue.async { self.analyze(sample, arrival: arrival, epoch: current, phase: phase, sampleCost: cost) }
    }

    private func analyze(_ sample: [Int16], arrival: Double, epoch frameEpoch: Int, phase framePhase: Phase,
                         sampleCost: Double) {
        lock.lock()
        let previous = epoch == frameEpoch ? self.previous : nil
        lock.unlock()
        let started = Self.nowMs()
        let turn = previous.flatMap { Self.turn(Self.shiftErrors($0, sample)) }
        let cost = Self.nowMs() - started

        var line: String?
        lock.lock()
        if !loggedCost, previous != nil {
            loggedCost = true
            line = String(format: "trial frame cost: sample %.2f ms (render callback), correlate %.2f ms", sampleCost, cost)
        }
        defer {
            lock.unlock()
            if let line { LabLog.write(line) }
        }
        guard epoch == frameEpoch, phase != .idle else { return }
        frames += 1
        if framePhase == .baseline {
            baselineMoved = baselineMoved || turn != nil
            self.previous = sample
            return
        }
        guard let best = turn else { self.previous = sample; return }
        if best >= 12 {
            candidate = nil
            self.previous = sample
            return
        }
        guard let polled = polledAt, let sent = sentAt, arrival > sent else { self.previous = sample; return }
        if let first = candidate, arrival - first <= 85 {
            phase = .idle
            turns.append(first - polled)
            sends.append(sent - polled)
            let result = String(format: "trial %d/12 poll→turn %.1f ms (poll→send %.2f ms, shift %d)",
                                attempts, first - polled, sent - polled, best - 12)
            line = line.map { $0 + "\n" + result } ?? result
            return
        }
        candidate = arrival
        self.previous = sample
    }

    // MARK: - Detection (trial.rs)

    static func nowMs() -> Double { CACurrentMediaTime() * 1000 }

    /// Sparse luma grid over the NV12 Y plane: rows 14–61%, columns 10–90%.
    static func sample(_ buffer: CVPixelBuffer) -> [Int16]? {
        guard CVPixelBufferGetPlaneCount(buffer) >= 2 else { return nil }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0) else { return nil }
        let width = CVPixelBufferGetWidthOfPlane(buffer, 0)
        let height = CVPixelBufferGetHeightOfPlane(buffer, 0)
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let bytes = base.assumingMemoryBound(to: UInt8.self)
        let xs = (0..<cols).map { Int((0.10 + 0.80 * Double($0) / Double(cols)) * Double(width)) }
        var pixels = [Int16](repeating: 0, count: rows * cols)
        pixels.withUnsafeMutableBufferPointer { out in
            xs.withUnsafeBufferPointer { xs in
                for r in 0..<rows {
                    let y = Int((0.14 + 0.47 * Double(r) / Double(rows)) * Double(height))
                    let row = bytes + y * stride
                    for c in 0..<cols { out[r * cols + c] = Int16(row[xs[c]]) }
                }
            }
        }
        return pixels
    }

    /// Sum of absolute differences for horizontal shifts -12...12.
    static func shiftErrors(_ previous: [Int16], _ current: [Int16]) -> [Int] {
        var errors = [Int](repeating: 0, count: 25)
        errors.withUnsafeMutableBufferPointer { errors in
            previous.withUnsafeBufferPointer { a in
                current.withUnsafeBufferPointer { b in
                    for r in Swift.stride(from: 5, to: rows - 5, by: 4) {
                        for c in Swift.stride(from: 13, to: cols - 13, by: 3) {
                            let base = r * cols + c
                            let value = Int(a[base])
                            for shift in 0..<25 {
                                errors[shift] += abs(value - Int(b[base + shift - 12]))
                            }
                        }
                    }
                }
            }
        }
        return errors
    }

    /// Turn when the unshifted error is above the noise floor and the best
    /// shift improves it by at least 18%. Returns the best shift index.
    static func turn(_ errors: [Int]) -> Int? {
        let zero = errors[12]
        guard zero > 1600 else { return nil }
        let best = errors.indices.min { errors[$0] < errors[$1] }!
        return Double(zero - errors[best]) / Double(zero) >= 0.18 ? best : nil
    }

    /// The Rust test vectors: identical frames are no turn; stripes shifted
    /// right by 4 columns give index 16.
    static func selfCheck() -> Bool {
        let flat = [Int16](repeating: 7, count: rows * cols)
        var a = [Int16](repeating: 0, count: rows * cols)
        var b = a
        for r in 0..<rows {
            for c in 0..<cols {
                a[r * cols + c] = (c / 16) % 2 == 0 ? 200 : 20
                let shifted = c - 4
                b[r * cols + c] = shifted >= 0 && (shifted / 16) % 2 == 0 ? 200 : 20
            }
        }
        return turn(shiftErrors(flat, flat)) == nil && turn(shiftErrors(a, b)) == 16
    }
}
