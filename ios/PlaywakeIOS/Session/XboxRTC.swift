import AVFoundation
import Foundation
import QuartzCore
import WebRTC

/// The live console/cloud peer connection.
///
/// Mirrors the desktop media worker: recvonly audio+video transceivers, four
/// data channels with Xbox protocol names, an SDP offer munged with `b=AS`,
/// one-shot ICE exchange through the Rust core, the message-channel handshake,
/// and an 8 ms input loop on the `input` channel.
final class XboxRTC: NSObject {
    struct Options {
        var stun: String?
        var bitrateKbps: UInt32 = 20_000
        var width: UInt32 = 1280
        var height: UInt32 = 720
    }

    enum Event {
        case state(String)
        case inputReady
        case inputSent
    }

    var onEvent: ((Event) -> Void)?
    var onStats: ((LabStats) -> Void)?

    private let renderer: LowLatencyVideoRenderer
    private let core = PlaywakeCore.shared
    private let factory: RTCPeerConnectionFactory
    private var pc: RTCPeerConnection?
    private var options = Options()
    private var startTime: Double = 0
    private var handsize: (UInt32, UInt32) = (1280, 720)

    private var messageChannel: RTCDataChannel?
    private var controlChannel: RTCDataChannel?
    private var inputChannel: RTCDataChannel?
    private var rendererTrack: RTCVideoTrack?
    private var rendererAttached = false

    private var localCandidates: [[String: Any]] = []
    private var localGatheringDone = false
    private var remoteDescriptionSet = false
    private var exchanged = false
    private var pendingRemote: [RTCIceCandidate] = []

    private var inputTimer: DispatchSourceTimer?
    private var statsTimer: DispatchSourceTimer?
    private var inputQueue = DispatchQueue(label: "io.playwake.input", qos: .userInteractive)
    private var sequence: UInt32 = 0
    private var handshakeOpened = false
    private var handshakeAcknowledged = false

    private var lastInbound: (frames: Double, bytes: Double, at: CFAbsoluteTime)?
    private var stats = LabStats()

    init(renderer: LowLatencyVideoRenderer) {
        self.renderer = renderer
        RTCSetMinDebugLogLevel(.warning)
        self.factory = RTCPeerConnectionFactory(
            encoderFactory: RTCDefaultVideoEncoderFactory(),
            decoderFactory: RTCDefaultVideoDecoderFactory())
        super.init()
    }

    // MARK: - Lifecycle

    func start(_ options: Options) {
        self.options = options
        self.handsize = (options.width, options.height)
        self.startTime = CACurrentMediaTime() * 1000
        configureAudio()

        let config = RTCConfiguration()
        config.sdpSemantics = .unifiedPlan
        config.bundlePolicy = .maxBundle
        config.continualGatheringPolicy = .gatherOnce
        config.tcpCandidatePolicy = .disabled
        config.disableLinkLocalNetworks = true
        config.maxIPv6Networks = 1
        config.iceCheckMinInterval = 20
        if let stun = options.stun {
            config.iceServers = [RTCIceServer(urlStrings: [stun])]
        } else {
            config.iceServers = []
        }
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        guard let pc = factory.peerConnection(with: config, constraints: constraints, delegate: self) else {
            emit(.state("could not create peer connection"))
            return
        }
        self.pc = pc
        emit(.state("negotiating"))

        addReceivers(on: pc)
        createChannels(on: pc)
        pc.offer(for: constraints) { [weak self] offer, error in
            guard let self else { return }
            guard let offer else {
                self.fail("offer failed: \(error?.localizedDescription ?? "unknown")")
                return
            }
            let munged = self.core.mungeOffer(offer.sdp, bitrate: options.bitrateKbps, captureTime: false)?.sdp
                ?? offer.sdp
            let local = RTCSessionDescription(type: .offer, sdp: munged)
            pc.setLocalDescription(local) { error in
                guard error == nil else {
                    self.fail("setLocalDescription: \(error!.localizedDescription)")
                    return
                }
                self.exchangeOffer(munged)
            }
        }
    }

    func stop() {
        inputTimer?.cancel(); inputTimer = nil
        statsTimer?.cancel(); statsTimer = nil
        messageChannel?.close()
        controlChannel?.close()
        inputChannel?.close()
        rendererTrack?.remove(renderer)
        rendererAttached = false
        rendererTrack = nil
        pc?.close()
        pc = nil
        emit(.state("stopped"))
    }

    private func configureAudio() {
        let session = RTCAudioSession.sharedInstance()
        session.lockForConfiguration()
        let configuration = RTCAudioSessionConfiguration.webRTC()
        configuration.category = AVAudioSession.Category.playback.rawValue
        configuration.categoryOptions = []
        configuration.mode = AVAudioSession.Mode.moviePlayback.rawValue
        configuration.ioBufferDuration = 0.005
        _ = try? session.setConfiguration(configuration, active: true)
        session.unlockForConfiguration()
        session.isAudioEnabled = true
    }

    private func createChannels(on pc: RTCPeerConnection) {
        for (label, proto) in [("input", "1.0"), ("control", "controlV1"),
                               ("message", "messageV1"), ("chat", "chatV1")] {
            let configuration = RTCDataChannelConfiguration()
            configuration.isOrdered = true
            configuration.protocol = proto
            let channel = pc.dataChannel(forLabel: label, configuration: configuration)
            channel?.delegate = self
            switch label {
            case "message": messageChannel = channel
            case "control": controlChannel = channel
            case "input": inputChannel = channel
            default: break
            }
        }
    }

    private func addReceivers(on pc: RTCPeerConnection) {
        let audio = RTCRtpTransceiverInit()
        audio.direction = .recvOnly
        _ = pc.addTransceiver(of: .audio, init: audio)

        let video = RTCRtpTransceiverInit()
        video.direction = .recvOnly
        if let transceiver = pc.addTransceiver(of: .video, init: video) {
            preferH264(transceiver)
        }
    }

    private func preferH264(_ transceiver: RTCRtpTransceiver) {
        let capabilities = factory.rtpReceiverCapabilities(forKind: kRTCMediaStreamTrackKindVideo)
        let h264 = capabilities.codecs.filter { $0.mimeType == "video/H264" }.sorted { a, b in
            let aBaseline = (a.parameters["profile-level-id"] ?? "").hasPrefix("42")
            let bBaseline = (b.parameters["profile-level-id"] ?? "").hasPrefix("42")
            return aBaseline && !bBaseline
        }
        guard !h264.isEmpty else { return }
        do {
            // The non-deprecated ObjC selector imports its NSError slot as `()`.
            try transceiver.setCodecPreferences(h264, error: ())
        } catch {
            emit(.state("codec preference failed: \(error.localizedDescription)"))
        }
    }

    // MARK: - Signaling

    private func exchangeOffer(_ offer: String) {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            do {
                let answer = try self.core.answer(offer: offer).get()
                DispatchQueue.main.async {
                    guard let pc = self.pc else { return }
                    let description = RTCSessionDescription(type: .answer, sdp: answer)
                    pc.setRemoteDescription(description) { error in
                        guard error == nil else {
                            self.fail("setRemoteDescription: \(error!.localizedDescription)")
                            return
                        }
                        self.remoteDescriptionSet = true
                        self.flushRemote()
                        self.emit(.state("answer set"))
                        self.exchangeCandidatesIfReady()
                    }
                }
            } catch {
                self.fail("SDP exchange: \(error)")
            }
        }
    }

    private func exchangeCandidatesIfReady() {
        guard remoteDescriptionSet, localGatheringDone, !exchanged else { return }
        exchanged = true
        let local = localCandidates
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            switch self.core.exchange(candidates: local) {
            case .success(let remote):
                DispatchQueue.main.async {
                    let candidates = remote.compactMap { entry -> RTCIceCandidate? in
                        var text = entry.candidate
                        while text.hasPrefix("a=") { text.removeFirst(2) }
                        guard text.hasPrefix("candidate:"), !text.contains("end-of-candidates") else { return nil }
                        return RTCIceCandidate(sdp: text, sdpMLineIndex: Int32(entry.line ?? 0), sdpMid: entry.mid)
                    }
                    for candidate in candidates { self.add(candidate) }
                    self.emit(.state("candidates exchanged (\(local.count) → \(candidates.count))"))
                }
            case .failure(let error):
                self.emit(.state("ICE exchange: \(error)"))
            }
        }
    }

    private func add(_ candidate: RTCIceCandidate) {
        guard let pc else { return }
        if remoteDescriptionSet {
            pc.add(candidate) { error in
                if let error { LabLog.write("add remote candidate failed: \(error.localizedDescription)") }
            }
        } else {
            pendingRemote.append(candidate)
        }
    }

    private func flushRemote() {
        guard remoteDescriptionSet else { return }
        for candidate in pendingRemote { add(candidate) }
        pendingRemote.removeAll()
    }

    // MARK: - Handshake / input

    private func sendHandshakeOpen() {
        guard !handshakeOpened, let handshake = core.handshake(width: handsize.0, height: handsize.1) else { return }
        handshakeOpened = true
        send(text: handshake.open, on: messageChannel)
    }

    private func acknowledgeHandshake() {
        guard !handshakeAcknowledged, let handshake = core.handshake(width: handsize.0, height: handsize.1) else { return }
        handshakeAcknowledged = true
        for message in handshake.messages {
            switch message.label {
            case "control": send(text: message.json, on: controlChannel)
            case "message": send(text: message.json, on: messageChannel)
            default: break
            }
        }
        let packet = core.connectPacket(startTime: startTime)
        send(data: packet, on: inputChannel)
        startInput()
        emit(.inputReady)
    }

    private func startInput() {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.inputTimer == nil else { return }
            let timer = DispatchSource.makeTimerSource(queue: self.inputQueue)
            timer.schedule(deadline: .now(), repeating: .milliseconds(8), leeway: .milliseconds(0))
            timer.setEventHandler { [weak self] in self?.tick() }
            timer.resume()
            self.inputTimer = timer
            self.emit(.state("input armed"))
        }
    }

    private func tick() {
        guard let channel = inputChannel, channel.readyState == .open else { return }
        let gamepad = Gamepad.shared.snapshot()
        sequence &+= 1
        let time = CACurrentMediaTime() * 1000 - startTime
        let packet = core.inputPacket(
            sequence: sequence, time: time, buttons: gamepad.buttons,
            left: (gamepad.axes[0], gamepad.axes[1]),
            right: (gamepad.axes[2], gamepad.axes[3]),
            triggers: (gamepad.lt, gamepad.rt))
        send(data: packet, on: channel)
    }

    private func send(data: Data, on channel: RTCDataChannel?) {
        guard let channel, channel.readyState == .open else { return }
        channel.sendData(RTCDataBuffer(data: data, isBinary: true))
    }

    private func send(text: String, on channel: RTCDataChannel?) {
        guard let channel, channel.readyState == .open, let data = text.data(using: .utf8) else { return }
        channel.sendData(RTCDataBuffer(data: data, isBinary: true))
    }

    // MARK: - Media

    private func attachRenderer(_ track: RTCMediaStreamTrack?, mid: String) {
        guard !rendererAttached, let video = track as? RTCVideoTrack else { return }
        video.add(renderer)
        rendererAttached = true
        rendererTrack = video
        emit(.state("video \(mid)"))
    }

    // MARK: - Stats

    private func startStats() {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.statsTimer == nil else { return }
            let timer = DispatchSource.makeTimerSource(queue: self.inputQueue)
            timer.schedule(deadline: .now() + 1, repeating: 1.0, leeway: .milliseconds(50))
            timer.setEventHandler { [weak self] in self?.collectStats() }
            timer.resume()
            self.statsTimer = timer
        }
    }

    private func collectStats() {
        guard let pc else { return }
        let now = CFAbsoluteTimeGetCurrent()
        pc.statistics { [weak self] report in
            guard let self else { return }
            var updated = self.stats
            updated.renderFps = self.renderer.snapshotFPS()
            updated.latencyMs = self.renderer.snapshotLatency()
            for (_, stat) in report.statistics {
                let values = stat.values
                guard stat.type == "inbound-rtp", values["kind"] as? String == "video" else { continue }
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
                let delay = self.number(values["jitterBufferDelay"])
                updated.jitterBufferMs = emitted > 0 ? delay / emitted * 1000 : 0
                updated.width = Int(self.number(values["frameWidth"]))
                updated.height = Int(self.number(values["frameHeight"]))
            }
            for (_, stat) in report.statistics where stat.type == "candidate-pair" {
                if let state = stat.values["state"] as? String, state == "succeeded" {
                    let rtt = self.number(stat.values["currentRoundTripTime"])
                    if rtt > 0 { updated.rttMs = rtt * 1000 }
                }
            }
            self.stats = updated
            DispatchQueue.main.async { [weak self] in self?.onStats?(updated) }
        }
    }

    private func number(_ value: NSObject?) -> Double {
        (value as? NSNumber)?.doubleValue ?? 0
    }

    // MARK: - Helpers

    private func emit(_ event: Event) {
        DispatchQueue.main.async { [weak self] in self?.onEvent?(event) }
    }

    private func fail(_ message: String) {
        emit(.state(message))
        LabLog.write("xbox-rtc FATAL \(message)")
    }
}

extension XboxRTC: RTCPeerConnectionDelegate {
    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        emit(.state("ice \(newState.rawValue)"))
        if newState == .failed { emit(.state("ICE failed")) }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {
        emit(.state("gathering \(newState.rawValue)"))
        if newState == .complete {
            localGatheringDone = true
            exchangeCandidatesIfReady()
        }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        var text = candidate.sdp
        while text.hasPrefix("a=") { text.removeFirst(2) }
        guard text.hasPrefix("candidate:") else { return }
        var entry: [String: Any] = ["candidate": text, "sdpMLineIndex": Int(candidate.sdpMLineIndex)]
        if let mid = candidate.sdpMid { entry["sdpMid"] = mid }
        localCandidates.append(entry)
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        emit(.state("channel \(dataChannel.label) open"))
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didStartReceivingOn transceiver: RTCRtpTransceiver) {
        guard transceiver.mediaType == .video else { return }
        attachRenderer(transceiver.receiver.track, mid: transceiver.mid ?? "-")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd receiver: RTCRtpReceiver, streams: [RTCMediaStream]) {
        if receiver.track is RTCVideoTrack {
            attachRenderer(receiver.track, mid: "-")
        }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        emit(.state("pc \(newState.rawValue)"))
        if newState == .connected {
            if !rendererAttached {
                let videos = peerConnection.transceivers.filter { $0.mediaType == .video }
                if let transceiver = videos.first(where: { !($0.mid ?? "").isEmpty }) ?? videos.first {
                    attachRenderer(transceiver.receiver.track, mid: transceiver.mid ?? "-")
                }
            }
            startStats()
        }
        if newState == .failed { emit(.state("peer connection failed")) }
    }
}

extension XboxRTC: RTCDataChannelDelegate {
    func dataChannelDidChangeState(_ dataChannel: RTCDataChannel) {
        if dataChannel.label == "message" && dataChannel.readyState == .open {
            sendHandshakeOpen()
        }
    }

    func dataChannel(_ dataChannel: RTCDataChannel, didReceiveMessageWith buffer: RTCDataBuffer) {
        guard let text = String(data: buffer.data, encoding: .utf8) else { return }
        if dataChannel.label == "message", core.isHandshakeAck(text) {
            acknowledgeHandshake()
        }
    }
}
