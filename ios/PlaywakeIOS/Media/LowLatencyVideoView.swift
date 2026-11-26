import AVFoundation
import QuartzCore
import SwiftUI
import WebRTC

struct LowLatencyVideoView: UIViewRepresentable {
    let renderer: LowLatencyVideoRenderer

    func makeUIView(context: Context) -> DisplayView {
        let view = DisplayView()
        view.renderer = renderer
        if let sink = view.caMetalSink {
            renderer.attachCAMetal(sink)
        } else if let metal = view.metalView {
            renderer.attachMetal(metal)
        } else {
            renderer.attach(view.displayLayer)
        }
        return view
    }

    func updateUIView(_ uiView: DisplayView, context: Context) {
        uiView.renderer = renderer
    }
}

final class DisplayView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }

    var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }
    /// `-metal`: render through libwebrtc's Metal view instead of the sample
    /// buffer layer, to A/B the Catalyst display path.
    private(set) var metalView: RTCMTLVideoView?
    /// `-cametal`: custom CAMetalLayer sink with display-sync control.
    private(set) var caMetalSink: CAMetalVideoSink?
    private var metalHost: MetalHostView?
    weak var renderer: LowLatencyVideoRenderer?
    private var displayLink: CADisplayLink?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
        displayLayer.preventsDisplaySleepDuringVideoPlayback = true
        let args = ProcessInfo.processInfo.arguments
        if args.contains("-cametal") {
            let host = MetalHostView(frame: bounds)
            host.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            addSubview(host)
            metalHost = host
            let defaults = UserDefaults.standard
            let vsyncOn = defaults.object(forKey: "vsyncOn") == nil
                ? true : defaults.bool(forKey: "vsyncOn")
            caMetalSink = CAMetalVideoSink(layer: host.metalLayer,
                                           displaySync: vsyncOn && !args.contains("-vsyncOff"))
        } else if args.contains("-metal") {
            let metal = RTCMTLVideoView(frame: bounds)
            metal.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            // The internal MTKView defaults to aspect-fill (cropped zoom);
            // videoContentMode is the only knob that reaches it.
            metal.videoContentMode = .scaleAspectFit
            addSubview(metal)
            metalView = metal
        }
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil {
            stopDisplayLink()
        } else {
            startDisplayLink()
        }
    }

    deinit {
        displayLink?.invalidate()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        displayLayer.frame = bounds
        if let host = metalHost {
            let scale = window?.screen.nativeScale ?? 2
            caMetalSink?.setDrawableSize(CGSize(width: host.bounds.width * scale,
                                                height: host.bounds.height * scale))
        }
    }

    private func startDisplayLink() {
        guard displayLink == nil else { return }
        let link = CADisplayLink(target: self, selector: #selector(displayTick(_:)))
        // ProMotion panels pick the highest rate only when asked; every frame
        // then waits at most one 120 Hz vsync (~8.3 ms) instead of 60 Hz (~16.7 ms).
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        displayLink = link
    }

    private func stopDisplayLink() {
        displayLink?.invalidate()
        displayLink = nil
        tickCount = 0
        tickIntervalSum = 0
        tickWindowStart = 0
    }

    private var tickCount = 0
    private var tickIntervalSum: CFTimeInterval = 0
    private var tickWindowStart: CFTimeInterval = 0

    @objc private func displayTick(_ link: CADisplayLink) {
        renderer?.recordDisplayTick(link.targetTimestamp)
        // Logs the display's frame interval every 5 s. It comes from the
        // link's own timestamps, so a busy main thread does not skew it.
        tickCount += 1
        tickIntervalSum += link.targetTimestamp - link.timestamp
        if tickWindowStart == 0 { tickWindowStart = link.timestamp }
        if link.timestamp - tickWindowStart >= 5 {
            let interval = tickIntervalSum / Double(tickCount)
            LabLog.write(String(format: "display %.1f Hz (frame %.2f ms)", 1 / interval, interval * 1000))
            tickCount = 0
            tickIntervalSum = 0
            tickWindowStart = link.timestamp
        }
    }
}

final class MetalHostView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
}
