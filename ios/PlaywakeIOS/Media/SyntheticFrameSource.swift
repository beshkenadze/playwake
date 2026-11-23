import CoreGraphics
import CoreText
import CoreVideo
import Foundation
import QuartzCore

/// Generates NV12 frames on a high-precision timer and hands them to WebRTC's
/// video source. NV12 avoids a pixel-format conversion (and a VideoToolbox
/// compression-session reset) inside the H.264 encoder. In the lab this stands
/// in for the Xbox H.264 stream; it lets us measure the full encode ->
/// transport -> decode -> display path inside one process.
final class SyntheticFrameSource {
    let width: Int
    let height: Int
    let fps: Int
    var onFrame: ((CVPixelBuffer, Int64) -> Void)?

    private let queue = DispatchQueue(label: "io.playwake.frames", qos: .userInteractive)
    private var timer: DispatchSourceTimer?
    private var pool: CVPixelBufferPool?
    private var frameNumber = 0

    init(width: Int, height: Int, fps: Int) {
        self.width = width
        self.height = height
        self.fps = fps
    }

    func start() {
        guard timer == nil else { return }
        createPool()
        let source = DispatchSource.makeTimerSource(queue: queue)
        source.schedule(deadline: .now(), repeating: 1.0 / Double(fps), leeway: .milliseconds(0))
        source.setEventHandler { [weak self] in self?.tick() }
        source.resume()
        timer = source
        LabLog.write("frame source started \(width)x\(height)@\(fps) nv12")
    }

    func stop() {
        timer?.cancel()
        timer = nil
    }

    private func createPool() {
        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
            kCVPixelBufferIOSurfacePropertiesKey as String: [:],
        ]
        var pool: CVPixelBufferPool?
        CVPixelBufferPoolCreate(kCFAllocatorDefault, nil, attributes as CFDictionary, &pool)
        self.pool = pool
    }

    private func tick() {
        guard let pool else { return }
        var buffer: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &buffer) == kCVReturnSuccess,
              let buffer else { return }
        let hostTime = Int64(CACurrentMediaTime() * 1_000_000_000)
        draw(into: buffer, number: frameNumber, hostTime: hostTime)
        frameNumber += 1
        onFrame?(buffer, hostTime)
    }

    private func draw(into buffer: CVPixelBuffer, number: Int, hostTime: Int64) {
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
              let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) else { return }
        let lumaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let chromaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        let chromaHeight = CVPixelBufferGetHeightOfPlane(buffer, 1)

        // Neutral chroma keeps the synthetic pattern grayscale.
        memset(chroma, 128, chromaStride * chromaHeight)

        let space = CGColorSpaceCreateDeviceGray()
        guard let context = CGContext(data: luma, width: width, height: height,
                                      bitsPerComponent: 8, bytesPerRow: lumaStride,
                                      space: space, bitmapInfo: CGImageAlphaInfo.none.rawValue) else { return }

        let w = CGFloat(width)
        let h = CGFloat(height)
        context.setFillColor(CGColor(gray: 0.22, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: w, height: h))

        let bandHeight = h / 8
        context.setFillColor(CGColor(gray: 0.32, alpha: 1))
        context.fill(CGRect(x: 0, y: bandHeight, width: w, height: bandHeight))

        // Moving vertical marker gives an immediate visual phase reference.
        let phase = CGFloat(number % width)
        context.setFillColor(CGColor(gray: 1.0, alpha: 1))
        context.fill(CGRect(x: phase, y: 0, width: 6, height: h))

        let font = CTFontCreateWithName("Menlo-Bold" as CFString, CGFloat(height) * 0.22, nil)
        let attributes: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: CGColor(gray: 1.0, alpha: 1),
        ]
        let line = CTLineCreateWithAttributedString(
            NSAttributedString(string: String(format: "%06d", number), attributes: attributes))
        context.textPosition = CGPoint(x: CGFloat(width) * 0.04, y: CGFloat(height) * 0.06)
        CTLineDraw(line, context)

        let captionFont = CTFontCreateWithName("Menlo-Regular" as CFString, CGFloat(height) * 0.045, nil)
        let caption = CTLineCreateWithAttributedString(
            NSAttributedString(string: "PLAYWAKE LOW-LATENCY LAB", attributes: [
                .font: captionFont,
                .foregroundColor: CGColor(gray: 0.9, alpha: 1),
            ]))
        context.textPosition = CGPoint(x: CGFloat(width) * 0.04, y: CGFloat(height) * 0.78)
        CTLineDraw(caption, context)

        drawBarcode(in: context, hostTime: hostTime)
    }

    /// 32 white/black blocks encode the capture time in milliseconds. The
    /// renderer reads them straight from decoded pixels, so latency does not
    /// depend on clock sync between the two peer connections.
    private func drawBarcode(in context: CGContext, hostTime: Int64) {
        let value = UInt32(truncatingIfNeeded: hostTime / 1_000_000)
        let blockWidth: CGFloat = 12
        let blockHeight: CGFloat = 16
        let originX: CGFloat = 16
        for rowY in [CGFloat(8), CGFloat(height) - blockHeight - 8] {
            for bit in 0..<32 {
                let on = (value >> UInt32(31 - bit)) & 1 == 1
                context.setFillColor(CGColor(gray: on ? 1.0 : 0.0, alpha: 1))
                context.fill(CGRect(x: originX + CGFloat(bit) * blockWidth,
                                    y: rowY,
                                    width: blockWidth - 2,
                                    height: blockHeight))
            }
        }
    }
}
