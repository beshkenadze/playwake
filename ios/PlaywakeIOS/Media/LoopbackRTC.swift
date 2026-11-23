import Foundation
import WebRTC

/// In-process WebRTC loopback: one peer connection encodes synthetic frames,
/// the other receives, decodes with the default (VideoToolbox) decoder and
/// hands frames to the low-latency renderer. A data channel carries 8 ms
/// pings so we can measure transport RTT with the same cadence as the
/// Xbox input stream.
final class LoopbackRTC: NSObject {
    weak var delegate: LoopbackRTCDelegate?
    private(set) var stats = LabStats()

    private let factory: RTCPeerConnectionFactory
    private let source: RTCVideoSource
    private let track: RTCVideoTrack
    private let capturer: RTCVideoCapturer
    private let frames: SyntheticFrameSource
    private let renderer: LowLatencyVideoRenderer

    private var senderPC: RTCPeerConnection?
    private var receiverPC: RTCPeerConnection?
    private var senderChannel: RTCDataChannel?
    private var receiverChannel: RTCDataChannel?
    private var rendererAttached = false
    private var rendererTrack: RTCVideoTrack?
    private var started = false

    private var senderRemoteSet = false
    private var receiverRemoteSet = false
    private var senderPending: [RTCIceCandidate] = []
    private var receiverPending: [RTCIceCandidate] = []
    private var captured = 0

    private let pingQueue = DispatchQueue(label: "io.playwake.ping", qos: .userInteractive)
    private var pingTimer: DispatchSourceTimer?
    private var statsTimer: DispatchSourceTimer?
    private var rttLock = NSLock()
    private var rttSamples: [Double] = []
    private var lastInbound: (frames: Double, bytes: Double, at: CFAbsoluteTime)?
    private var lastOutbound: (frames: Double, bytes: Double, at: CFAbsoluteTime)?

    init(renderer: LowLatencyVideoRenderer) {
        self.renderer = renderer
        WebRTCTrials.install()
        RTCSetMinDebugLogLevel(.warning)
        self.factory = RTCPeerConnectionFactory(encoderFactory: RTCDefaultVideoEncoderFactory(),
                                                decoderFactory: RTCDefaultVideoDecoderFactory())
        #if targetEnvironment(simulator)
        // The simulator shares the Mac's network stack but not the host's
        // local-network permission, so en0 traffic to itself is dropped.
        // Virtual bridge/VPN adapters loop back and connect immediately.
        let options = RTCPeerConnectionFactoryOptions()
        options.ignoreWiFiNetworkAdapter = true
        options.ignoreEthernetNetworkAdapter = true
        factory.setOptions(options)
        #endif
        self.source = factory.videoSource()
        self.track = factory.videoTrack(with: source, trackId: "playwake-video")
        self.capturer = RTCVideoCapturer(delegate: source)
        self.frames = SyntheticFrameSource(width: 1280, height: 720, fps: 60)
        super.init()
        source.adaptOutputFormat(toWidth: 1280, height: 720, fps: 60)
        frames.onFrame = { [weak self] pixelBuffer, hostTime in
            guard let self else { return }
            self.captured += 1
            let buffer = RTCCVPixelBuffer(pixelBuffer: pixelBuffer)
            let frame = RTCVideoFrame(buffer: buffer, rotation: ._0, timeStampNs: hostTime)
            self.source.capturer(self.capturer, didCapture: frame)
        }
    }

    func start() {
        guard !started else { return }
        started = true
        LabLog.write("loopback start")

        let config = RTCConfiguration()
        config.sdpSemantics = .unifiedPlan
        config.bundlePolicy = .maxBundle
        config.continualGatheringPolicy = .gatherContinually
        config.tcpCandidatePolicy = .disabled
        config.disableLinkLocalNetworks = true
        config.maxIPv6Networks = 1
        config.iceCheckMinInterval = 20
        config.iceServers = []
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)

        senderPC = factory.peerConnection(with: config, constraints: constraints, delegate: self)
        receiverPC = factory.peerConnection(with: config, constraints: constraints, delegate: self)
        guard let senderPC, let receiverPC else {
            finish("could not create peer connections")
            return
        }

        let channelConfig = RTCDataChannelConfiguration()
        channelConfig.isOrdered = true
        channelConfig.protocol = "1.0"
        senderChannel = senderPC.dataChannel(forLabel: "input", configuration: channelConfig)
        senderChannel?.delegate = self

        let sendInit = RTCRtpTransceiverInit()
        sendInit.direction = .sendOnly
        let senderTransceiver = senderPC.addTransceiver(with: track, init: sendInit)
        if let senderTransceiver {
            preferH264(transceiver: senderTransceiver, sending: true)
            let parameters = senderTransceiver.sender.parameters
            if let encoding = parameters.encodings.first {
                encoding.minBitrateBps = 2_000_000
                encoding.maxBitrateBps = 12_000_000
                encoding.maxFramerate = 60
                senderTransceiver.sender.parameters = parameters
            }
        }

        // The offer already carries a video m-line; applying it creates the
        // receiving transceiver. Adding one up front yields a second, never
        // used video transceiver and the renderer attaches to the wrong track.

        senderPC.offer(for: constraints) { [weak self] offer, error in
            guard let self else { return }
            guard let offer else { self.finish("offer failed: \(error?.localizedDescription ?? "unknown")"); return }
            LabLog.write("offer created (\(offer.sdp.count) bytes)")
            self.senderPC?.setLocalDescription(offer) { error in
                guard error == nil else { self.finish("sender setLocal failed: \(error!.localizedDescription)"); return }
                self.receiverPC?.setRemoteDescription(offer) { error in
                    guard error == nil else { self.finish("receiver setRemote failed: \(error!.localizedDescription)"); return }
                    self.receiverRemoteSet = true
                    self.logCandidates(in: offer.sdp, label: "offer")
                    self.logCandidates(in: self.receiverPC?.remoteDescription?.sdp, label: "receiver remote")
                    self.flushPending()
                    self.receiverPC?.answer(for: constraints) { answer, error in
                        guard let answer else { self.finish("answer failed: \(error?.localizedDescription ?? "unknown")"); return }
                        self.receiverPC?.setLocalDescription(answer) { error in
                            guard error == nil else { self.finish("receiver setLocal failed: \(error!.localizedDescription)"); return }
                            self.senderPC?.setRemoteDescription(answer) { error in
                                guard error == nil else { self.finish("sender setRemote failed: \(error!.localizedDescription)"); return }
                                self.senderRemoteSet = true
                                self.logCandidates(in: answer.sdp, label: "answer")
                                self.logCandidates(in: self.senderPC?.remoteDescription?.sdp, label: "sender remote")
                                self.flushPending()
                                LabLog.write("negotiation complete")
                                self.startMedia()
                            }
                        }
                    }
                }
            }
        }
    }

    func stop() {
        pingTimer?.cancel(); pingTimer = nil
        statsTimer?.cancel(); statsTimer = nil
        frames.stop()
        senderChannel?.close()
        receiverChannel?.close()
        senderPC?.close()
        receiverPC?.close()
        senderPC = nil
        receiverPC = nil
        senderChannel = nil
        receiverChannel = nil
        started = false
        setState("stopped")
        LabLog.write("loopback stopped")
    }

    private func startMedia() {
        frames.start()
        startPing()
        startStats()
        setState("connected")
    }

    private func preferH264(transceiver: RTCRtpTransceiver?, sending: Bool) {
        guard let transceiver else { return }
        let capabilities = sending
            ? factory.rtpSenderCapabilities(forKind: kRTCMediaStreamTrackKindVideo)
            : factory.rtpReceiverCapabilities(forKind: kRTCMediaStreamTrackKindVideo)
        let h264 = capabilities.codecs.filter { $0.mimeType == "video/H264" }
        let ordered = h264.sorted { a, b in
            let aBaseline = (a.parameters["profile-level-id"] ?? "").hasPrefix("42")
            let bBaseline = (b.parameters["profile-level-id"] ?? "").hasPrefix("42")
            return aBaseline && !bBaseline
        }
        guard !ordered.isEmpty else {
            LabLog.write("H264 not available in \(sending ? "encoder" : "decoder") capabilities")
            return
        }
        do {
            try transceiver.setCodecPreferences(ordered, error: ())
            LabLog.write("codec preference: \(ordered.map { $0.mimeType + " " + ($0.parameters["profile-level-id"] ?? "") }.joined(separator: ", "))")
        } catch {
            LabLog.write("setCodecPreferences failed: \(error.localizedDescription)")
        }
    }

    private func attachRenderer(_ track: RTCMediaStreamTrack?, mid: String) {
        guard !rendererAttached, let video = track as? RTCVideoTrack else { return }
        video.add(renderer)
        rendererAttached = true
        rendererTrack = video
        LabLog.write("renderer attached to receiver video track id=\(video.trackId) mid=\(mid) ptr=\(Unmanaged.passUnretained(video).toOpaque())")
    }

    private func attachRendererIfNeeded(_ pc: RTCPeerConnection) {
        guard pc === receiverPC, !rendererAttached else { return }
        let videos = pc.transceivers.filter { $0.mediaType == .video }
        let transceiver = videos.first { !($0.mid ?? "").isEmpty } ?? videos.first
        guard let transceiver else {
            LabLog.write("attach skipped: no receiver video transceiver yet")
            return
        }
        attachRenderer(transceiver.receiver.track, mid: transceiver.mid ?? "-")
    }

    private func logReceiverTracks() {
        guard let receiverPC else { return }
        for transceiver in receiverPC.transceivers {
            let track = transceiver.receiver.track
            let video = track as? RTCVideoTrack
            LabLog.write("receiver transceiver mid=\(transceiver.mid ?? "-") type=\(transceiver.mediaType.rawValue) direction=\(transceiver.direction.rawValue) track=\(video?.trackId ?? "none") enabled=\(track?.isEnabled ?? false) ptr=\(video.map { Unmanaged.passUnretained($0).toOpaque() } ?? nil)")
        }
    }

    private func add(candidate: RTCIceCandidate, to pc: RTCPeerConnection?, label: String) {
        pc?.add(candidate) { error in
            if let error { LabLog.write("add candidate to \(label) failed: \(error.localizedDescription)") }
        }
    }

    private func flushPending() {
        if senderRemoteSet {
            for candidate in senderPending { add(candidate: candidate, to: senderPC, label: "sender") }
            senderPending.removeAll()
        }
        if receiverRemoteSet {
            for candidate in receiverPending { add(candidate: candidate, to: receiverPC, label: "receiver") }
            receiverPending.removeAll()
        }
    }

    private func logCandidates(in sdp: String?, label: String) {
        guard let sdp else { return }
        let lines = sdp.split(separator: "\n").filter { $0.hasPrefix("a=candidate:") }
        LabLog.write("\(label) SDP candidates (\(lines.count)): \(lines.joined(separator: " || "))")
    }

    // MARK: - Ping / stats

    private func startPing() {
        let timer = DispatchSource.makeTimerSource(queue: pingQueue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(8), leeway: .milliseconds(0))
        timer.setEventHandler { [weak self] in self?.sendPing() }
        timer.resume()
        pingTimer = timer
    }

    private func sendPing() {
        guard let channel = senderChannel, channel.readyState == .open else { return }
        var data = Data([1])
        var now = CACurrentMediaTime()
        withUnsafeBytes(of: &now) { data.append(contentsOf: $0) }
        channel.sendData(RTCDataBuffer(data: data, isBinary: true))
    }

    private func startStats() {
        let timer = DispatchSource.makeTimerSource(queue: pingQueue)
        timer.schedule(deadline: .now() + 1, repeating: 1.0, leeway: .milliseconds(50))
        timer.setEventHandler { [weak self] in self?.collectStats() }
        timer.resume()
        statsTimer = timer
    }

    private func collectStats() {
        guard let receiverPC else { return }
        let now = CFAbsoluteTimeGetCurrent()
        receiverPC.statistics { [weak self] report in
            guard let self else { return }
            var updated = self.stats
            updated.renderFps = self.renderer.snapshotFPS()
            updated.latencyMs = self.renderer.snapshotLatency()

            for (_, stat) in report.statistics {
                let values = stat.values
                guard let kind = values["kind"] as? String, kind == "video" else { continue }
                switch stat.type {
                case "inbound-rtp":
                    let frames = self.number(values["framesDecoded"])
                    let bytes = self.number(values["bytesReceived"])
                    if let previous = self.lastInbound {
                        let dt = max(now - previous.at, 0.001)
                        updated.decodeFps = (frames - previous.frames) / dt
                        updated.bitrateMbps = (bytes - previous.bytes) * 8 / dt / 1_000_000
                    }
                    self.lastInbound = (frames, bytes, now)
                    updated.jitterMs = self.number(values["jitter"]) * 1000
                    updated.framesDropped = Int(self.number(values["framesDropped"]))
                    let decodeTime = self.number(values["totalDecodeTime"])
                    updated.decodeMs = frames > 0 ? decodeTime / frames * 1000 : 0
                    let emitted = self.number(values["jitterBufferEmittedCount"])
                    let jbDelay = self.number(values["jitterBufferDelay"])
                    updated.jitterBufferMs = emitted > 0 ? jbDelay / emitted * 1000 : 0
                    updated.width = Int(self.number(values["frameWidth"]))
                    updated.height = Int(self.number(values["frameHeight"]))
                case "candidate-pair":
                    if let state = values["state"] as? String, state == "succeeded" {
                        let rtt = self.number(values["currentRoundTripTime"])
                        if rtt > 0 { updated.rttMs = rtt * 1000 }
                    }
                default:
                    break
                }
            }

            self.rttLock.lock()
            if !self.rttSamples.isEmpty {
                updated.rttMs = self.rttSamples.reduce(0, +) / Double(self.rttSamples.count)
                self.rttSamples.removeAll(keepingCapacity: true)
            }
            self.rttLock.unlock()

            guard let senderPC = self.senderPC else {
                self.publish(updated)
                return
            }
            senderPC.statistics { [weak self] senderReport in
                guard let self else { return }
                var encoded = 0.0
                var sent = 0.0
                for (_, stat) in senderReport.statistics {
                    let values = stat.values
                    guard stat.type == "outbound-rtp",
                          let kind = values["kind"] as? String, kind == "video" else { continue }
                    let frames = self.number(values["framesEncoded"])
                    let bytes = self.number(values["bytesSent"])
                    if let previous = self.lastOutbound {
                        let dt = max(now - previous.at, 0.001)
                        updated.encodeFps = (frames - previous.frames) / dt
                    }
                    let encodeTime = self.number(values["totalEncodeTime"])
                    updated.encodeMs = frames > 0 ? encodeTime / frames * 1000 : 0
                    self.lastOutbound = (frames, bytes, now)
                    encoded = frames
                    sent = bytes
                }
                let captured = self.captured
                LabLog.write("media cap \(captured) enc \(Int(encoded)) sent \(Int(sent))")
                self.logReceiverTracks()
                self.publish(updated)
            }
        }
    }

    private func publish(_ updated: LabStats) {
        var updated = updated
        updated.state = "connected"
        self.stats = updated
        self.delegate?.loopback(self, didUpdate: updated)
    }

    private func number(_ value: NSObject?) -> Double {
        (value as? NSNumber)?.doubleValue ?? 0
    }

    private func setState(_ state: String) {
        stats.state = state
        delegate?.loopback(self, didChange: state)
        LabLog.write("state: \(state)")
    }

    private func finish(_ reason: String) {
        LabLog.write("FATAL \(reason)")
        setState(reason)
        stop()
    }
}

protocol LoopbackRTCDelegate: AnyObject {
    func loopback(_ rtc: LoopbackRTC, didUpdate stats: LabStats)
    func loopback(_ rtc: LoopbackRTC, didChange state: String)
}

extension LoopbackRTC: RTCPeerConnectionDelegate {
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        LabLog.write("ice \(newState.rawValue)")
        if newState == .failed { finish("ICE failed") }
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {
        let source = peerConnection === senderPC ? "sender" : "receiver"
        LabLog.write("ice gathering \(source) \(newState.rawValue)")
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        let source = peerConnection === senderPC ? "sender" : "receiver"
        LabLog.write("\(source) candidate \(candidate.sdp)")
        if peerConnection === senderPC {
            if senderRemoteSet { add(candidate: candidate, to: receiverPC, label: "receiver") } else { senderPending.append(candidate) }
        } else if peerConnection === receiverPC {
            if receiverRemoteSet { add(candidate: candidate, to: senderPC, label: "sender") } else { receiverPending.append(candidate) }
        }
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        LabLog.write("data channel opened: \(dataChannel.label) protocol=\(dataChannel.protocol)")
        if dataChannel.label == "input" {
            receiverChannel = dataChannel
            dataChannel.delegate = self
        }
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didStartReceivingOn transceiver: RTCRtpTransceiver) {
        LabLog.write("didStartReceivingOn mid=\(transceiver.mid ?? "-") type=\(transceiver.mediaType.rawValue)")
        if peerConnection === receiverPC, transceiver.mediaType == .video {
            attachRenderer(transceiver.receiver.track, mid: transceiver.mid ?? "-")
        }
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd receiver: RTCRtpReceiver, streams: [RTCMediaStream]) {
        LabLog.write("didAddReceiver track=\(receiver.track?.trackId ?? "none")")
        attachRendererIfNeeded(peerConnection)
    }
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        LabLog.write("pc \(newState.rawValue)")
        if newState == .connected { attachRendererIfNeeded(peerConnection) }
    }
}

extension LoopbackRTC: RTCDataChannelDelegate {
    func dataChannelDidChangeState(_ dataChannel: RTCDataChannel) {
        LabLog.write("channel \(dataChannel.label) state \(dataChannel.readyState.rawValue)")
        if dataChannel.label == "input" && dataChannel.readyState == .open && dataChannel === receiverChannel {
            LabLog.write("receiver input channel open")
        }
    }

    func dataChannel(_ dataChannel: RTCDataChannel, didReceiveMessageWith buffer: RTCDataBuffer) {
        let data = buffer.data
        guard let kind = data.first else { return }
        if kind == 1 {
            var echo = Data([2])
            echo.append(data.dropFirst())
            dataChannel.sendData(RTCDataBuffer(data: echo, isBinary: true))
        } else if kind == 2, data.count == 9 {
            var sent = 0.0
            withUnsafeMutableBytes(of: &sent) { destination in
                data.dropFirst().prefix(8).withUnsafeBytes { source in
                    destination.copyBytes(from: source)
                }
            }
            let rtt = (CACurrentMediaTime() - sent) * 1000
            rttLock.lock()
            rttSamples.append(rtt)
            rttLock.unlock()
        }
    }
}
