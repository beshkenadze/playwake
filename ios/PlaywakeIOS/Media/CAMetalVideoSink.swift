import CoreVideo
import Metal
import QuartzCore
import WebRTC

/// Custom CAMetalLayer sink: draws the decoder's NV12 IOSurface straight to a
/// drawable with no copy, keeps no frame queue, and exposes displaySyncEnabled
/// (the knob AVSampleBufferDisplayLayer and RTCMTLVideoView both hide).
/// Design follows moonlight-qt's Metal renderer (read for design only, GPLv3).
final class CAMetalVideoSink {
    private let layer: CAMetalLayer
    private let device: MTLDevice
    private let commandQueue: MTLCommandQueue
    private var textureCache: CVMetalTextureCache?
    private let pipeline: MTLRenderPipelineState
    private let drawQueue = DispatchQueue(label: "io.playwake.cametal", qos: .userInteractive)
    private let stateLock = NSLock()
    private var displaySync: Bool
    private var drawablePending = false
    private(set) var droppedFrames = 0
    /// Seconds from submit to the frame reaching the display, per frame.
    var onPresented: ((CFTimeInterval) -> Void)?

    /// Live toggle for the UI setting; the drawable wait follows the mode.
    func setDisplaySync(_ on: Bool) {
        stateLock.lock()
        displaySync = on
        stateLock.unlock()
        #if os(macOS) || targetEnvironment(macCatalyst)
        layer.displaySyncEnabled = on
        #endif
        LabLog.write("cametal displaySync:\(on)")
    }

    /// `displaySync == false` presents immediately (tearing possible, lowest
    /// latency); with sync on, presents align to vsync and the drawable wait
    /// is capped at half a frame interval.
    init?(layer: CAMetalLayer, displaySync: Bool) {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue() else { return nil }
        self.layer = layer
        self.device = device
        self.commandQueue = queue
        self.displaySync = displaySync
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        #if os(macOS) || targetEnvironment(macCatalyst)
        layer.displaySyncEnabled = displaySync
        #endif
        layer.maximumDrawableCount = 2
        layer.colorspace = CGColorSpace(name: CGColorSpace.sRGB)
        layer.backgroundColor = CGColor(gray: 0, alpha: 1)

        var cache: CVMetalTextureCache?
        guard CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache) == kCVReturnSuccess,
              let cache else { return nil }
        self.textureCache = cache

        guard let library = try? device.makeLibrary(source: Self.shaderSource, options: nil),
              let vertex = library.makeFunction(name: "vshader"),
              let fragment = library.makeFunction(name: "fshader") else { return nil }
        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = vertex
        desc.fragmentFunction = fragment
        desc.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: desc) else { return nil }
        self.pipeline = pipeline
        LabLog.write("cametal sink ready displaySync:\(displaySync)")
    }

    func setDrawableSize(_ size: CGSize) {
        layer.drawableSize = size
    }

    /// Draws and presents the frame synchronously on the caller thread, except
    /// the drawable acquisition which is capped at 8 ms; a busy drawable drops
    /// the frame instead of queueing it.
    func render(_ frame: RTCVideoFrame) {
        guard let cv = frame.buffer as? RTCCVPixelBuffer else { return }
        let pixelBuffer = cv.pixelBuffer
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        guard width > 0, height > 0,
              CVPixelBufferGetPlaneCount(pixelBuffer) >= 2,
              let cache = textureCache else { return }

        stateLock.lock()
        if drawablePending {
            droppedFrames += 1
            stateLock.unlock()
            return
        }
        drawablePending = true
        stateLock.unlock()

        var drawable: CAMetalDrawable?
        let semaphore = DispatchSemaphore(value: 0)
        drawQueue.async { [layer] in
            drawable = layer.nextDrawable()
            semaphore.signal()
        }
        let got = semaphore.wait(timeout: .now() + .milliseconds(displaySync ? 16 : 8)) == .success
        stateLock.lock()
        drawablePending = false
        if !got || drawable == nil { droppedFrames += 1 }
        stateLock.unlock()
        guard got, let drawable else { return }

        let videoRange = CVPixelBufferGetPixelFormatType(pixelBuffer) == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        guard let yTexture = planeTexture(cache, pixelBuffer, plane: 0, format: .r8Unorm,
                                          width: width, height: height),
              let uvTexture = planeTexture(cache, pixelBuffer, plane: 1, format: .rg8Unorm,
                                           width: width / 2, height: height / 2) else { return }

        let drawableSize = layer.drawableSize
        let videoAspect = Double(width) / Double(height)
        let drawableAspect = drawableSize.width / max(drawableSize.height, 1)
        // Aspect-fit letterbox in clip space.
        var scale = SIMD2<Float>(1, 1)
        if videoAspect > drawableAspect {
            scale.y = Float(drawableAspect / videoAspect)
        } else {
            scale.x = Float(videoAspect / drawableAspect)
        }
        var rotation = Int32(frame.rotation.rawValue)
        var matrix = Self.fullRange709
        var offset = Self.fullRange709Offset
        if videoRange {
            matrix = Self.videoRange709
            offset = Self.videoRange709Offset
        }

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store

        guard let commandBuffer = commandQueue.makeCommandBuffer(),
              let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else { return }
        encoder.setRenderPipelineState(pipeline)
        encoder.setVertexBytes(&scale, length: MemoryLayout<SIMD2<Float>>.stride, index: 0)
        encoder.setFragmentTexture(yTexture, index: 0)
        encoder.setFragmentTexture(uvTexture, index: 1)
        encoder.setFragmentBytes(&matrix, length: MemoryLayout<Float>.stride * 9, index: 0)
        encoder.setFragmentBytes(&offset, length: MemoryLayout<Float>.stride * 3, index: 1)
        encoder.setFragmentBytes(&rotation, length: MemoryLayout<Int32>.stride, index: 2)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        #if !targetEnvironment(simulator)
        // The simulator SDK has no presented handler.
        let submitted = CACurrentMediaTime()
        let onPresented = self.onPresented
        drawable.addPresentedHandler { drawable in
            if drawable.presentedTime > 0 { onPresented?(drawable.presentedTime - submitted) }
        }
        #endif
        commandBuffer.present(drawable)
        commandBuffer.commit()
    }

    private func planeTexture(_ cache: CVMetalTextureCache, _ buffer: CVPixelBuffer,
                              plane: Int, format: MTLPixelFormat,
                              width: Int, height: Int) -> MTLTexture? {
        var texture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, buffer, nil, format, width, height, plane, &texture)
        guard status == kCVReturnSuccess, let texture else { return nil }
        return CVMetalTextureGetTexture(texture)
    }

    // BT.709 coefficients; HD console streams are 709.
    private static let fullRange709: [Float] = [
        1, 0, 1.5748,
        1, -0.1873, -0.4681,
        1, 1.8556, 0,
    ]
    private static let fullRange709Offset: [Float] = [-0.7874, 0.3277, -0.9278]
    private static let videoRange709: [Float] = [
        1.164, 0, 1.793,
        1.164, -0.213, -0.533,
        1.164, 2.112, 0,
    ]
    private static let videoRange709Offset: [Float] = [-0.9695, 0.3005, -1.129]

    private static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;
    struct VOut {
        float4 position [[position]];
        float2 texCoord;
    };
    vertex VOut vshader(uint vid [[vertex_id]],
                        constant float2 &scale [[buffer(0)]]) {
        float2 pos[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
        VOut out;
        out.position = float4(pos[vid] * scale, 0, 1);
        out.texCoord = float2(pos[vid].x * 0.5 + 0.5, 0.5 - pos[vid].y * 0.5);
        return out;
    }
    fragment float4 fshader(VOut in [[stage_in]],
                            texture2d<float> yTex [[texture(0)]],
                            texture2d<float> uvTex [[texture(1)]],
                            constant float *m [[buffer(0)]],
                            constant float *o [[buffer(1)]],
                            constant int &rotation [[buffer(2)]]) {
        constexpr sampler s(filter::linear, address::clamp_to_edge);
        float2 uv = in.texCoord;
        if (rotation == 90) {
            uv = float2(1.0 - uv.y, uv.x);
        } else if (rotation == 180) {
            uv = float2(1.0 - uv.x, 1.0 - uv.y);
        } else if (rotation == 270) {
            uv = float2(uv.y, 1.0 - uv.x);
        }
        float y = yTex.sample(s, uv).r;
        float2 chroma = uvTex.sample(s, uv).rg;
        float3 yuv = float3(y, chroma.x, chroma.y);
        float3x3 mat = float3x3(float3(m[0], m[3], m[6]),
                                float3(m[1], m[4], m[7]),
                                float3(m[2], m[5], m[8]));
        float3 rgb = mat * yuv + float3(o[0], o[1], o[2]);
        return float4(clamp(rgb, 0.0, 1.0), 1.0);
    }
    """
}