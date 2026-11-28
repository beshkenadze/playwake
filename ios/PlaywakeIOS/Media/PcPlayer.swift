import AVFoundation
import VideoToolbox
import WebRTC

/// Plays a stream from a PC. The core runs the stream; this decodes its
/// frames with VideoToolbox into the shared renderer, plays its Opus sound
/// and sends the pad through the same input route as an Xbox stream.
final class PcPlayer {
    let renderer: LowLatencyVideoRenderer
    /// The first decoded frame, on the main queue.
    var onFirstFrame: (() -> Void)?
    /// Stream-menu events from the input route, on the main queue.
    var onMenu: ((UInt32) -> Void)?

    /// The core's session of this stream; 0 until it starts.
    private(set) var session: UInt64 = 0
    private var format: CMVideoFormatDescription?
    private var decoder: VTDecompressionSession?
    private var decoded = false
    private let sound = OpusPlayer()
    private let route = pw_route_new()
    private let inputQueue = DispatchQueue(label: "io.playwake.pc.input", qos: .userInteractive)
    private var inputTimer: DispatchSourceTimer?

    init(renderer: LowLatencyVideoRenderer) {
        self.renderer = renderer
    }

    deinit {
        pw_route_free(route)
    }

    /// False when the core refused the stream.
    func start(game id: String, width: UInt32, height: UInt32, fps: UInt32, bitrateKbps: UInt32) -> Bool {
        Self.playing = self
        sound.start()
        Gamepad.shared.start()
        startInput()
        session = PlaywakeCore.shared.streamPc(game: id, width: width, height: height, fps: fps, bitrateKbps: bitrateKbps,
                                     video: { data, length, sets, lengths, count, hevc, _ in
                                         guard let data else { return }
                                         PcPlayer.playing?.decode(data, length, sets: sets, lengths: lengths,
                                                                  count: count, hevc: hevc)
                                     },
                                     audio: { data, length in
                                         guard let data else { return }
                                         PcPlayer.playing?.sound.play(data, length)
                                     })
        if session == 0 { stop() }
        return session != 0
    }

    func stop() {
        // Returns once the stream's thread is done, so no frame or sound follows.
        pw_pc_stop()
        inputQueue.sync {
            inputTimer?.cancel()
            inputTimer = nil
        }
        sound.stop()
        if let decoder {
            VTDecompressionSessionWaitForAsynchronousFrames(decoder)
            VTDecompressionSessionInvalidate(decoder)
        }
        decoder = nil
        format = nil
        if Self.playing === self { Self.playing = nil }
    }

    func openMenu() {
        inputQueue.async { [weak self] in
            guard let self else { return }
            self.report(pw_route_open(self.route))
        }
    }

    /// `guide` presses the PC's Guide button once the pad is released.
    func closeMenu(guide: Bool) {
        inputQueue.async { [weak self] in
            guard let self else { return }
            self.report(pw_route_close(self.route, guide))
        }
    }

    // MARK: - Video

    /// The player the core's C callbacks reach: one PC stream at a time.
    private static let lock = NSLock()
    nonisolated(unsafe) private static var current: PcPlayer?
    private static var playing: PcPlayer? {
        get { lock.lock(); defer { lock.unlock() }; return current }
        set { lock.lock(); current = newValue; lock.unlock() }
    }

    /// Called on the stream's thread, one frame after another.
    private func decode(_ data: UnsafePointer<UInt8>, _ length: Int, sets: UnsafePointer<UnsafePointer<UInt8>?>?,
                        lengths: UnsafePointer<Int>?, count: Int, hevc: Bool) {
        if count > 0, let sets, let lengths {
            describe((0..<count).compactMap { sets[$0] }, sizes: (0..<count).map { lengths[$0] }, hevc: hevc)
        }
        // Until the first key frame there is nothing to decode against.
        guard let format, let decoder else { return }
        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: length,
                                                 blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
                                                 offsetToData: 0, dataLength: length, flags: 0,
                                                 blockBufferOut: &block) == noErr, let block,
              CMBlockBufferReplaceDataBytes(with: data, blockBuffer: block, offsetIntoDestination: 0,
                                            dataLength: length) == noErr else { return }
        var sample: CMSampleBuffer?
        var size = length
        guard CMSampleBufferCreateReady(allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
                                        sampleCount: 1, sampleTimingEntryCount: 0, sampleTimingArray: nil,
                                        sampleSizeEntryCount: 1, sampleSizeArray: &size,
                                        sampleBufferOut: &sample) == noErr, let sample else { return }
        // Asynchronous, so a slow decode does not hold up the sound behind it.
        VTDecompressionSessionDecodeFrame(decoder, sampleBuffer: sample, flags: [._EnableAsynchronousDecompression],
                                          infoFlagsOut: nil) {
            [weak self] status, _, image, _, _ in
            guard status == noErr, let image else { return }
            self?.show(image)
        }
    }

    /// A key frame's parameter sets: a new decoder when they describe a new format.
    private func describe(_ sets: [UnsafePointer<UInt8>], sizes: [Int], hevc: Bool) {
        var described: CMVideoFormatDescription?
        let status = sets.withUnsafeBufferPointer { sets in
            sizes.withUnsafeBufferPointer { sizes in
                hevc
                    ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault, parameterSetCount: sets.count,
                        parameterSetPointers: sets.baseAddress!, parameterSetSizes: sizes.baseAddress!,
                        nalUnitHeaderLength: 4, extensions: nil, formatDescriptionOut: &described)
                    : CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: kCFAllocatorDefault, parameterSetCount: sets.count,
                        parameterSetPointers: sets.baseAddress!, parameterSetSizes: sizes.baseAddress!,
                        nalUnitHeaderLength: 4, formatDescriptionOut: &described)
            }
        }
        guard status == noErr, let described else {
            LabLog.write("pc: parameter sets rejected (\(status))")
            return
        }
        if let format, CMFormatDescriptionEqual(format, otherFormatDescription: described) { return }
        if let decoder {
            VTDecompressionSessionWaitForAsynchronousFrames(decoder)
            VTDecompressionSessionInvalidate(decoder)
        }
        // Until a decoder for the new format exists, nothing decodes: the
        // next key frame tries again, also if it brings the old format back.
        decoder = nil
        format = nil
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
        ]
        var made: VTDecompressionSession?
        let created = VTDecompressionSessionCreate(allocator: kCFAllocatorDefault, formatDescription: described,
                                                   decoderSpecification: nil,
                                                   imageBufferAttributes: attributes as CFDictionary,
                                                   outputCallback: nil, decompressionSessionOut: &made)
        guard created == noErr, let made else {
            LabLog.write("pc: no decoder for the stream (\(created))")
            return
        }
        VTSessionSetProperty(made, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        format = described
        decoder = made
        let size = CMVideoFormatDescriptionGetDimensions(described)
        LabLog.write("pc: decoding \(hevc ? "HEVC" : "H.264") \(size.width)x\(size.height)")
        DispatchQueue.main.async { [renderer] in
            renderer.setSize(CGSize(width: Int(size.width), height: Int(size.height)))
        }
    }

    private func show(_ image: CVPixelBuffer) {
        let frame = RTCVideoFrame(buffer: RTCCVPixelBuffer(pixelBuffer: image), rotation: ._0,
                                  timeStampNs: Int64(CACurrentMediaTime() * 1_000_000_000))
        renderer.renderFrame(frame)
        if !decoded {
            decoded = true
            DispatchQueue.main.async { [weak self] in self?.onFirstFrame?() }
        }
    }

    // MARK: - Input

    private func startInput() {
        inputQueue.async { [weak self] in
            guard let self, self.inputTimer == nil else { return }
            let timer = DispatchSource.makeTimerSource(queue: self.inputQueue)
            timer.schedule(deadline: .now(), repeating: .milliseconds(8), leeway: .milliseconds(0))
            timer.setEventHandler { [weak self] in self?.sendPad() }
            timer.resume()
            self.inputTimer = timer
        }
    }

    private func sendPad() {
        let pad = Gamepad.shared.snapshot()
        var wire = pw_pad(axes: (pad.axes[0], pad.axes[1], pad.axes[2], pad.axes[3]),
                          buttons: pad.buttons, lt: pad.lt, rt: pad.rt)
        let now = CACurrentMediaTime() * 1000
        // The on-screen pad joins here, so the menu pauses it too.
        pw_input_pad(&wire, now)
        let events = pw_route_input(route, &wire, now)
        pw_pc_gamepad(&wire)
        report(events)
    }

    private func report(_ events: UInt32) {
        guard events != 0 else { return }
        DispatchQueue.main.async { [weak self] in self?.onMenu?(events) }
    }
}

/// Plays the PC's sound: one 5 ms Opus packet after another, decoded by
/// Core Audio. At most a few packets wait, so the sound stays close to live.
private final class OpusPlayer {
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let pcm = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)!
    private let opus: AVAudioFormat = {
        var description = AudioStreamBasicDescription(mSampleRate: 48_000, mFormatID: kAudioFormatOpus,
                                                      mFormatFlags: 0, mBytesPerPacket: 0, mFramesPerPacket: 240,
                                                      mBytesPerFrame: 0, mChannelsPerFrame: 2, mBitsPerChannel: 0,
                                                      mReserved: 0)
        return AVAudioFormat(streamDescription: &description)!
    }()
    private lazy var converter = AVAudioConverter(from: opus, to: pcm)
    private let lock = NSLock()
    private var waiting = 0
    private var stopped = false
    /// Packets that may wait to be played: 40 ms.
    private let mostWaiting = 8

    func start() {
        #if os(iOS)
        try? AVAudioSession.sharedInstance().setCategory(.playback, mode: .moviePlayback)
        try? AVAudioSession.sharedInstance().setActive(true)
        #endif
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: pcm)
        do { try engine.start() } catch { LabLog.write("pc: no sound (\(error))") }
        node.play()
    }

    func stop() {
        lock.lock()
        stopped = true
        lock.unlock()
        node.stop()
        engine.stop()
        #if os(iOS)
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        #endif
    }

    /// Called on the stream's thread.
    func play(_ data: UnsafePointer<UInt8>, _ length: Int) {
        lock.lock()
        // Late sound is worse than a gap: drop what would wait too long.
        let take = !stopped && waiting < mostWaiting
        if take { waiting += 1 }
        lock.unlock()
        guard take else { return }
        guard let converter, let out = AVAudioPCMBuffer(pcmFormat: pcm, frameCapacity: 960) else { return done() }
        let packet = AVAudioCompressedBuffer(format: opus, packetCapacity: 1, maximumPacketSize: max(length, 1))
        packet.data.copyMemory(from: data, byteCount: length)
        packet.byteLength = UInt32(length)
        packet.packetCount = 1
        packet.packetDescriptions?[0] = AudioStreamPacketDescription(mStartOffset: 0, mVariableFramesInPacket: 0,
                                                                     mDataByteSize: UInt32(length))
        var given = false
        var error: NSError?
        converter.convert(to: out, error: &error) { _, status in
            if given {
                status.pointee = .noDataNow
                return nil
            }
            given = true
            status.pointee = .haveData
            return packet
        }
        lock.lock()
        let playing = !stopped && out.frameLength > 0
        lock.unlock()
        guard playing else { return done() }
        node.scheduleBuffer(out) { [weak self] in self?.done() }
    }

    private func done() {
        lock.lock()
        waiting -= 1
        lock.unlock()
    }
}
