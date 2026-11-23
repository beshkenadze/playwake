import AVFoundation
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
    private let renderQueue = DispatchQueue(label: "io.playwake.render", qos: .userInteractive)
    private let usesBarcode: Bool
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
    private(set) var lastFrameAt: CFAbsoluteTime = 0

    func attach(_ layer: AVSampleBufferDisplayLayer) {
        lock.lock()
        targetLayer = layer
        lock.unlock()
        LabLog.write("renderer attached to AVSampleBufferDisplayLayer responds renderFrame:\(responds(to: Selector(("renderFrame:")))) setSize:\(responds(to: Selector(("setSize:"))))")
    }

    @objc(setSize:) func setSize(_ size: CGSize) {
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

    private var callbacks = 0

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
            LabLog.write("renderFrame callback #1 frame=\(frame == nil ? "nil" : "buffer")")
        }
        guard let frame else { return }
        let nowNs = Int64(CACurrentMediaTime() * 1_000_000_000)
        let timeStampLatency = Double(nowNs - frame.timeStampNs) / 1_000_000
        let kind = String(describing: type(of: frame.buffer))
        lock.lock()
        let firstTime = bufferKinds.insert(kind).inserted
        lock.unlock()
        if firstTime { LabLog.write("decoded buffer kind: \(kind)  \(frame.width)x\(frame.height)") }

        guard let cvBuffer = frame.buffer as? RTCCVPixelBuffer else {
            lock.lock(); dropped += 1; lock.unlock()
            return
        }
        let decodedLatency = usesBarcode ? barcodeLatency(cvBuffer.pixelBuffer) : nil
        let plausibleTimestamp = timeStampLatency >= 0 && timeStampLatency < 1_000
        lock.lock()
        latencyMs = decodedLatency ?? (plausibleTimestamp ? timeStampLatency : 0)
        lock.unlock()

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
        frameCount += 1
        lastFrameAt = CFAbsoluteTimeGetCurrent()
        let now = lastFrameAt
        if now - windowStart >= 1.0 {
            frameFps = Double(frameCount) / (now - windowStart)
            frameCount = 0
            windowStart = now
        }
        let droppedNow = dropped
        lock.unlock()

        guard let layer else { return }
        renderQueue.async {
            if layer.status == .failed {
                layer.flush()
                LabLog.write("display layer failed, flushed")
            }
            layer.enqueue(sampleBuffer)
        }

        if now - lastLog > 5 {
            lastLog = now
            LabLog.write(String(format: "render fps %.1f  dropped %d", frameFps, droppedNow))
        }
    }
}
