import AVFoundation
import SwiftUI

struct LowLatencyVideoView: UIViewRepresentable {
    let renderer: LowLatencyVideoRenderer

    func makeUIView(context: Context) -> DisplayView {
        let view = DisplayView()
        renderer.attach(view.displayLayer)
        return view
    }

    func updateUIView(_ uiView: DisplayView, context: Context) {}
}

final class DisplayView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }

    var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
        displayLayer.preventsDisplaySleepDuringVideoPlayback = true
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

    override func layoutSubviews() {
        super.layoutSubviews()
        displayLayer.frame = bounds
    }
}
