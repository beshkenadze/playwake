import AVFoundation
import CoreImage
import CoreMedia
import Foundation
import QuartzCore
import WebRTC

/// Renders decoded WebRTC frames through AVSampleBufferDisplayLayer with
/// `DisplayImmediately`, bypassing any display-link scheduling. This is the
/// lowest-latency path we control beyond libwebrtc's own decoder.
final class LowLatencyVideoRenderer: NSObject, RTCVideoRenderer {
    private let lock = NSLock()
    private weak var targetLayer: AVSampleBufferDisplayLayer?
    private weak var metalView: RTCMTLVideoView?
    private var caMetalSink: CAMetalVideoSink?
    private let renderQueue = DispatchQueue(label: "io.playwake.render", qos: .userInteractive)
    private let usesBarcode: Bool
    /// `-sync`: enqueue on the decode callback thread instead of hopping to the
    /// render queue, so a slow enqueue throttles delivery instead of piling up.
    private let syncEnqueue = ProcessInfo.processInfo.arguments.contains("-sync")
    /// `-dropold`: flush queued samples before each enqueue so the layer only
    /// ever holds the newest frame (moonlight-qt's drop-old policy).
    private let dropOld = ProcessInfo.processInfo.arguments.contains("-dropold")
    private var formatDescription: CMVideoFormatDescription?
    private var formatSize: CGSize = .zero
    private var lastLog = CFAbsoluteTimeGetCurrent()
    private var bufferKinds: Set<String> = []

    /// `usesBarcode` is for the synthetic lab only: it stamps the capture time
    /// into each frame and the renderer reads it back to measure end-to-end
    /// latency without clock sync. Real console frames have no barcode and the
    /// read would be a wasted IOSurface readback per frame.
    init(usesBarcode: Bool = false) {
        self.usesBarcode = usesBarcode
        super.init()
    }

    private var frameCount = 0
    private var windowStart = CFAbsoluteTimeGetCurrent()
    private var frameFps = 0.0
    private var latencyMs = 0.0
    private var dropped = 0
    private var submitSum = 0.0
    private var submitCount = 0
    private var enqueuedAt: CFAbsoluteTime = 0
    private var presentSum = 0.0
    private var presentCount = 0
    private(set) var lastFrameAt: CFAbsoluteTime = 0
    private var trials: LatencyTrials?
    private var decodeClock: DecodeClock?
    private var holdSum = 0.0
    private var holdCount = 0

    func setDecodeClock(_ clock: DecodeClock?) {
        lock.lock(); decodeClock = clock; lock.unlock()
    }

    /// Average time from decode finish to renderer delivery, in milliseconds.
    func snapshotHold() -> Double {
        lock.lock()
        defer { lock.unlock() }
        let average = holdCount > 0 ? holdSum / Double(holdCount) * 1000 : 0
        holdSum = 0
        holdCount = 0
        return average
    }
    private var keepsLastFrame = false
    private var lastBuffer: CVPixelBuffer?

    func setTrials(_ trials: LatencyTrials?) {
        lock.lock(); self.trials = trials; lock.unlock()
    }

    /// Keeps a reference to the newest decoded frame for `writeSnapshot`.
    func keepLastFrame(_ on: Bool) {
        lock.lock()
        keepsLastFrame = on
        if !on { lastBuffer = nil }
        lock.unlock()
    }

    /// Writes the newest decoded frame as a 960 px wide JPEG.
    func writeSnapshot(to url: URL) -> Bool {
        lock.lock()
        let buffer = lastBuffer
        lock.unlock()
        guard let buffer else { return false }
        let image = CIImage(cvPixelBuffer: buffer)
        let scale = 960 / image.extent.width
        let scaled = image.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
              let data = CIContext().jpegRepresentation(of: scaled, colorSpace: colorSpace, options: [:])
        else { return false }
        return (try? data.write(to: url, options: .atomic)) != nil
    }

    func attach(_ layer: AVSampleBufferDisplayLayer) {
        lock.lock()
        targetLayer = layer
        lock.unlock()
        LabLog.write("renderer attached to AVSampleBufferDisplayLayer sync:\(syncEnqueue) dropold:\(dropOld) responds renderFrame:\(responds(to: Selector(("renderFrame:")))) setSize:\(responds(to: Selector(("setSize:"))))")
    }

    /// Metal sink: frames go straight from the decoder's IOSurface to a Metal
    /// drawable, skipping AVSampleBufferDisplayLayer's queue and compositor.
    func attachMetal(_ view: RTCMTLVideoView) {
        lock.lock()
        metalView = view
        lock.unlock()
        LabLog.write("renderer attached to RTCMTLVideoView (Metal)")
    }

    /// Custom CAMetalLayer sink with display-sync control; takes precedence
    /// over both the sample buffer layer and RTCMTLVideoView.
    func attachCAMetal(_ sink: CAMetalVideoSink) {
        // Metal reports when each frame reached the display, so `pres` is
        // measured rather than modelled from the display link.
        sink.onPresented = { [weak self] wait in
            guard let self, wait >= 0, wait < 0.1 else { return }
            self.lock.lock()
            self.presentSum += wait
            self.presentCount += 1
            self.lock.unlock()
        }
        lock.lock()
        caMetalSink = sink
        lock.unlock()
        LabLog.write("renderer attached to CAMetalLayer sink")
    }

    func setDisplaySync(_ on: Bool) {
        lock.lock()
        let sink = caMetalSink
        lock.unlock()
        sink?.setDisplaySync(on)
    }

    @objc(setSize:) func setSize(_ size: CGSize) {
        lock.lock()
        let metal = metalView
        lock.unlock()
        metal?.setSize(size)
        LabLog.write("renderer setSize \(Int(size.width))x\(Int(size.height))")
    }

    func snapshotFPS() -> Double {
        lock.lock()
        defer { lock.unlock() }
        return frameFps
    }

    func snapshotLatency() -> Double {
        lock.lock()
        defer { lock.unlock() }
        return latencyMs
    }

    /// Average time spent between the renderer callback and the sample buffer
    /// being handed to the display layer, in milliseconds.
    func snapshotSubmit() -> Double {
        lock.lock()
        defer { lock.unlock() }
        let average = submitCount > 0 ? submitSum / Double(submitCount) * 1000 : 0
        submitSum = 0
        submitCount = 0
        return average
    }

    /// Average wait between enqueueing the newest frame and the display link's
    /// next target timestamp, in milliseconds. This is the vsync part of the
    /// display latency; callback-to-enqueue is measured separately by
    /// ``snapshotSubmit()``.
    func snapshotPresent() -> Double {
        lock.lock()
        defer { lock.unlock() }
        let average = presentCount > 0 ? presentSum / Double(presentCount) * 1000 : 0
        presentSum = 0
        presentCount = 0
        return average
    }

    /// Called from the view's CADisplayLink. Counts each enqueued frame once.
    func recordDisplayTick(_ targetTimestamp: CFTimeInterval) {
        lock.lock()
        defer { lock.unlock() }
        guard enqueuedAt > 0 else {
            repeatTicks += 1
            if repeatTicks == 3 {
                LabLog.write("display stall: 3 vsyncs without a new frame")
            }
            return
        }
        repeatTicks = 0
        let wait = targetTimestamp - enqueuedAt
        if caMetalSink == nil, wait >= 0, wait < 0.1 {
            presentSum += wait
            presentCount += 1
        }
        enqueuedAt = 0
    }

    private var callbacks = 0
    private var gapCount = 0
    private var gapMax = 0.0

    /// Frame gaps over 25 ms at the renderer since the previous call.
    func snapshotGaps() -> (count: Int, maxMs: Double) {
        lock.lock()
        defer { lock.unlock() }
        let result = (gapCount, gapMax)
        gapCount = 0
        gapMax = 0
        return result
    }
    private var lastFrameEntry: CFAbsoluteTime = 0
    private var repeatTicks = 0

    /// Reads the capture timestamp encoded in the frame itself, so latency is
    /// measured from decoded pixels and does not depend on clock sync.
    private func barcodeLatency(_ buffer: CVPixelBuffer) -> Double? {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }

        let width = CVPixelBufferGetWidth(buffer)
        let height = CVPixelBufferGetHeight(buffer)
        let bgra = CVPixelBufferGetPlaneCount(buffer) == 0
        guard let base = bgra ? CVPixelBufferGetBaseAddress(buffer)
                              : CVPixelBufferGetBaseAddressOfPlane(buffer, 0) else { return nil }
        let stride = bgra ? CVPixelBufferGetBytesPerRow(buffer)
                          : CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let nowUs = UInt32(truncatingIfNeeded: Int64(CACurrentMediaTime() * 1_000_000))

        func luma(x: Int, y: Int) -> Int {
            let column = min(max(x, 0), width - 1)
            let row = min(max(y, 0), height - 1)
            let pointer = base.advanced(by: row * stride + column * (bgra ? 4 : 1))
            if bgra {
                let b = Int(pointer.load(fromByteOffset: 0, as: UInt8.self))
                let g = Int(pointer.load(fromByteOffset: 1, as: UInt8.self))
                let r = Int(pointer.load(fromByteOffset: 2, as: UInt8.self))
                return (r + g + b) / 3
            }
            return Int(pointer.load(as: UInt8.self))
        }

        var best: UInt32?
        // The barcode is mirrored top and bottom to be independent of row order.
        for rowCenter in [16.0 / 720.0, 704.0 / 720.0] {
            var value: UInt32 = 0
            for bit in 0..<32 {
                let centerX = (21.0 + 12.0 * Double(bit)) / 1280.0
                var total = 0
                var count = 0
                for dx in [-2.0, 0.0, 2.0] {
                    for dy in [-2.0, 0.0, 2.0] {
                        let x = Int((centerX + dx / 1280.0) * Double(width))
                        let y = Int((rowCenter + dy / 720.0) * Double(height))
                        total += luma(x: x, y: y)
                        count += 1
                    }
                }
                value = (value << 1) | UInt32(total / count >= 128 ? 1 : 0)
            }
            let delta = nowUs &- value
            if delta < 5_000_000 {
                best = best.map { min($0, delta) } ?? delta
            }
        }
        return best.map { Double($0) / 1_000 }
    }

    @objc(renderFrame:) func renderFrame(_ frame: RTCVideoFrame?) {
        callbacks += 1
        if callbacks == 1 {
            LabLog.write("renderFrame callback #1 frame=\(frame == nil ? "nil" : "buffer") timeStampNs=\(frame?.timeStampNs ?? -1)")
        }
        guard let frame else { return }
        let entryTime = CACurrentMediaTime()
        // Counted, not logged per gap: a log write here delays the next frame.
        let gapMs = lastFrameEntry > 0 ? (entryTime - lastFrameEntry) * 1000 : 0
        lastFrameEntry = entryTime
        let nowNs = Int64(entryTime * 1_000_000_000)
        let timeStampLatency = Double(nowNs - frame.timeStampNs) / 1_000_000
        let kind = String(describing: type(of: frame.buffer))
        lock.lock()
        let firstTime = bufferKinds.insert(kind).inserted
        // Over 2 s is the pause between two sessions, not a stall.
        if gapMs > 25, gapMs < 2000 {
            gapCount += 1
            gapMax = max(gapMax, gapMs)
        }
        lock.unlock()
        if firstTime { LabLog.write("decoded buffer kind: \(kind)  \(frame.width)x\(frame.height)") }

        guard let cvBuffer = frame.buffer as? RTCCVPixelBuffer else {
            lock.lock(); dropped += 1; lock.unlock()
            return
        }
        lock.lock()
        let trials = self.trials
        if keepsLastFrame { lastBuffer = cvBuffer.pixelBuffer }
        if let held = decodeClock?.held(frame.timeStamp), held >= 0, held < 0.5 {
            holdSum += held
            holdCount += 1
        }
        lock.unlock()
        trials?.observe(cvBuffer.pixelBuffer, arrival: entryTime * 1000)
        let decodedLatency = usesBarcode ? barcodeLatency(cvBuffer.pixelBuffer) : nil
        let plausibleTimestamp = timeStampLatency >= 0 && timeStampLatency < 1_000
        lock.lock()
        latencyMs = decodedLatency ?? (plausibleTimestamp ? timeStampLatency : 0)
        frameCount += 1
        lastFrameAt = CFAbsoluteTimeGetCurrent()
        let now = lastFrameAt
        if now - windowStart >= 1.0 {
            frameFps = Double(frameCount) / (now - windowStart)
            frameCount = 0
            windowStart = now
        }
        let droppedNow = dropped
        let metal = metalView
        let caSink = caMetalSink
        lock.unlock()

        if let sink = caSink {
            sink.render(frame)
            let submittedAt = CACurrentMediaTime()
            lock.lock()
            submitSum += submittedAt - entryTime
            submitCount += 1
            enqueuedAt = submittedAt
            lock.unlock()
            if now - lastLog > 5 {
                lastLog = now
                LabLog.write(String(format: "render fps %.1f  dropped %d  (cametal, sink drops %d)",
                                    frameFps, droppedNow, sink.droppedFrames))
            }
            return
        }

        if let metal {
            // RTCMTLVideoView skips frames whose timeStampNs equals the last
            // rendered one; with ForcePlayoutDelay=0 every frame arrives stamped
            // 0, so re-stamp with the unique callback time or the view stays black.
            let stamped = RTCVideoFrame(buffer: frame.buffer, rotation: frame.rotation, timeStampNs: nowNs)
            metal.renderFrame(stamped)
            let submittedAt = CACurrentMediaTime()
            lock.lock()
            submitSum += submittedAt - entryTime
            submitCount += 1
            enqueuedAt = submittedAt
            lock.unlock()
            if now - lastLog > 5 {
                lastLog = now
                LabLog.write(String(format: "render fps %.1f  dropped %d  (metal)", frameFps, droppedNow))
            }
            return
        }

        let width = CVPixelBufferGetWidth(cvBuffer.pixelBuffer)
        let height = CVPixelBufferGetHeight(cvBuffer.pixelBuffer)
        let size = CGSize(width: width, height: height)

        lock.lock()
        if formatSize != size || formatDescription == nil {
            var description: CMVideoFormatDescription?
            let status = CMVideoFormatDescriptionCreateForImageBuffer(
                allocator: kCFAllocatorDefault,
                imageBuffer: cvBuffer.pixelBuffer,
                formatDescriptionOut: &description)
            if status == noErr, let description {
                formatDescription = description
                formatSize = size
            }
        }
        let description = formatDescription
        lock.unlock()
        guard let description else { return }

        var timing = CMSampleTimingInfo(
            duration: CMTime(value: 1, timescale: 60),
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            decodeTimeStamp: .invalid)
        var sampleBuffer: CMSampleBuffer?
        let createStatus = CMSampleBufferCreateReadyWithImageBuffer(
            allocator: kCFAllocatorDefault,
            imageBuffer: cvBuffer.pixelBuffer,
            formatDescription: description,
            sampleTiming: &timing,
            sampleBufferOut: &sampleBuffer)
        guard createStatus == noErr, let sampleBuffer else { return }
        CMSetAttachment(sampleBuffer,
                        key: kCMSampleAttachmentKey_DisplayImmediately,
                        value: kCFBooleanTrue,
                        attachmentMode: kCMAttachmentMode_ShouldPropagate)

        lock.lock()
        let layer = targetLayer
        lock.unlock()

        guard let layer else { return }
        let enqueue = {
            if layer.status == .failed {
                layer.flush()
                LabLog.write("display layer failed, flushed")
            } else if self.dropOld {
                layer.flush()
            }
            layer.enqueue(sampleBuffer)
            let submittedAt = CACurrentMediaTime()
            self.lock.lock()
            self.submitSum += submittedAt - entryTime
            self.submitCount += 1
            self.enqueuedAt = submittedAt
            self.lock.unlock()
        }
        if syncEnqueue {
            enqueue()
        } else {
            renderQueue.async(execute: enqueue)
        }

        if now - lastLog > 5 {
            lastLog = now
            LabLog.write(String(format: "render fps %.1f  dropped %d", frameFps, droppedNow))
        }
    }
}
