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
        /// PW_ROUTE_* bits from the stream-input route.
        case menu(UInt32)
    }

    var onEvent: ((Event) -> Void)?
    var onStats: ((LabStats) -> Void)?

    private let renderer: LowLatencyVideoRenderer
    private let core = PlaywakeCore.shared
    private let factory: RTCPeerConnectionFactory
    private let decodeClock: DecodeClock
    private var lastCounters: [String: Double] = [:]
    private var loggedCandidates = false
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

    /// Owned by `inputQueue`, like everything the input loop touches.
    private var inputTimer: DispatchSourceTimer?
    private var lastSendAt: CFTimeInterval = 0
    /// Stream-input route shared with SteamOS; touched only on `inputQueue`.
    private let route = pw_route_new()
    private var stopped = false
    private var statsTimer: DispatchSourceTimer?
    private var inputQueue = DispatchQueue(label: "io.playwake.input", qos: .userInteractive)
    private var sequence: UInt32 = 0
    private var handshakeOpened = false
    private var handshakeAcknowledged = false
    private var inputIntervalMs = 8
    private var inputSentCount = 0
    private var inputLogAt: CFAbsoluteTime = 0
    private var sawControllerInput = false

    private var lastInbound: (frames: Double, bytes: Double, decode: Double, jbDelay: Double, jbEmitted: Double,
                              at: CFAbsoluteTime)?
    private var statsTicks = 0
    private var trials: LatencyTrials?
    private var remote: RemoteControl?
    private var stats = LabStats()

    init(renderer: LowLatencyVideoRenderer) {
        self.renderer = renderer
        WebRTCTrials.install()
        RTCSetMinDebugLogLevel(ProcessInfo.processInfo.arguments.contains("-rtcVerbose") ? .info : .warning)
        let decoders = TimedDecoderFactory()
        renderer.setDecodeClock(decoders.clock)
        self.decodeClock = decoders.clock
        self.factory = RTCPeerConnectionFactory(
            encoderFactory: RTCDefaultVideoEncoderFactory(),
            decoderFactory: decoders,
            audioDevice: PlayoutAudioDevice())
        super.init()
        let arguments = ProcessInfo.processInfo.arguments
        if arguments.contains("-latencyTrials") || arguments.contains("-remote") {
            let trials = LatencyTrials()
            trials.onActuate = { [weak self] in self?.sendNow() }
            self.trials = trials
            renderer.setTrials(trials)
            if arguments.contains("-remote") {
                remote = RemoteControl(renderer: renderer, onInput: { [weak self] in self?.sendNow() }) { [weak self] in
                    trials.run(height: Int(self?.options.height ?? 0))
                }
            }
        }
    }

    // MARK: - Lifecycle

    func start(_ options: Options) {
        self.options = options
        self.handsize = (options.width, options.height)
        self.startTime = CACurrentMediaTime() * 1000
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
        // First, so a handshake ACK still in flight cannot start input again.
        inputQueue.sync {
            stopped = true
            inputTimer?.cancel()
            inputTimer = nil
        }
        Gamepad.shared.onChange = nil
        remote?.stop()
        trials?.cancel()
        renderer.setTrials(nil)
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
        inputQueue.async { [weak self] in
            guard let self, !self.stopped, self.inputTimer == nil else { return }
            Gamepad.shared.onChange = { [weak self] in self?.sendNow() }
            self.remote?.start()
            if ProcessInfo.processInfo.arguments.contains("-latencyTrials") {
                self.trials?.run(height: Int(self.options.height))
            }
            let requested = UserDefaults.standard.integer(forKey: "inputMs")
            self.inputIntervalMs = (1...16).contains(requested) ? requested : 8
            self.inputLogAt = CFAbsoluteTimeGetCurrent()
            let timer = DispatchSource.makeTimerSource(queue: self.inputQueue)
            timer.schedule(deadline: .now(), repeating: .milliseconds(self.inputIntervalMs),
                           leeway: .milliseconds(0))
            timer.setEventHandler { [weak self] in
                // A tick already queued when sendNow() sent must not repeat it.
                guard let self, CACurrentMediaTime() - self.lastSendAt >= 0.001 else { return }
                self.tick()
            }
            timer.resume()
            self.inputTimer = timer
            self.emit(.state("input armed at \(self.inputIntervalMs) ms"))
        }
    }

    deinit {
        pw_route_free(route)
    }

    /// Opens the stream menu from touch or keyboard.
    func openMenu() {
        inputQueue.async { [weak self] in
            guard let self else { return }
            let events = pw_route_open(self.route)
            if events != 0 { self.emit(.menu(events)) }
        }
    }

    /// Closes the stream menu; `guide` presses Xbox Guide once the pad is released.
    func closeMenu(guide: Bool) {
        inputQueue.async { [weak self] in
            guard let self else { return }
            let events = pw_route_close(self.route, guide)
            if events != 0 { self.emit(.menu(events)) }
            self.sendNow()
        }
    }

    private func routePad(_ pad: inout Gamepad.Snapshot, at now: Double) -> UInt32 {
        var wire = pw_pad(axes: (pad.axes[0], pad.axes[1], pad.axes[2], pad.axes[3]),
                          buttons: pad.buttons, lt: pad.lt, rt: pad.rt)
        // Keyboard and mouse join as a pad here, so the menu pauses them too.
        pw_input_pad(&wire, now)
        let events = pw_route_input(route, &wire, now)
        pad.axes = [wire.axes.0, wire.axes.1, wire.axes.2, wire.axes.3]
        pad.buttons = wire.buttons
        pad.lt = wire.lt
        pad.rt = wire.rt
        return events
    }

    /// Sends the pad state now instead of at the next tick, then restarts the
    /// tick from this packet. Sends stay at least 1 ms apart.
    func sendNow() {
        inputQueue.async { [weak self] in
            guard let self, let timer = self.inputTimer else { return }
            let interval = DispatchTimeInterval.milliseconds(self.inputIntervalMs)
            let wait = 0.001 - (CACurrentMediaTime() - self.lastSendAt)
            if wait > 0 {
                // Too close to the last packet: send when the 1 ms is over.
                timer.schedule(deadline: .now() + wait, repeating: interval, leeway: .milliseconds(0))
                return
            }
            self.tick()
            timer.schedule(deadline: .now() + interval, repeating: interval, leeway: .milliseconds(0))
        }
    }

    private func tick() {
        guard let channel = inputChannel, channel.readyState == .open else { return }
        lastSendAt = CACurrentMediaTime()
        let polledAt = CACurrentMediaTime() * 1000
        var gamepad = Gamepad.shared.snapshot()
        // Only the physical pad goes through the route: the menu pauses it,
        // while remote-control and trial input still reach the console.
        let menuEvents = routePad(&gamepad, at: polledAt)
        remote?.apply(to: &gamepad, at: polledAt)
        let trialX = trials?.stickX(at: polledAt)
        if let trialX {
            gamepad.axes[2] = trialX
            gamepad.buttons = 0
        }
        sequence &+= 1
        let packet = core.inputPacket(
            sequence: sequence, time: polledAt - startTime, buttons: gamepad.buttons,
            left: (gamepad.axes[0], gamepad.axes[1]),
            right: (gamepad.axes[2], gamepad.axes[3]),
            triggers: (gamepad.lt, gamepad.rt))
        if send(data: packet, on: channel), trialX != nil {
            trials?.sent(polledAt: polledAt, sentAt: CACurrentMediaTime() * 1000)
        }

        if !sawControllerInput,
           gamepad.buttons != 0 || gamepad.lt > 0 || gamepad.rt > 0
               || gamepad.axes.contains(where: { $0 != 0 }) {
            sawControllerInput = true
            LabLog.write("input active buttons=\(gamepad.buttons) axes=\(gamepad.axes) lt=\(gamepad.lt) rt=\(gamepad.rt)")
        }
        if menuEvents != 0 { emit(.menu(menuEvents)) }
        inputSentCount += 1
        let now = CFAbsoluteTimeGetCurrent()
        if now - inputLogAt >= 1 {
            LabLog.write("input \(inputSentCount) packets/s interval \(inputIntervalMs) ms")
            inputSentCount = 0
            inputLogAt = now
        }
    }

    @discardableResult
    private func send(data: Data, on channel: RTCDataChannel?) -> Bool {
        guard let channel, channel.readyState == .open else { return false }
        return channel.sendData(RTCDataBuffer(data: data, isBinary: true))
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
            updated.submitMs = self.renderer.snapshotSubmit()
            updated.holdMs = self.renderer.snapshotHold()
            updated.presentMs = self.renderer.snapshotPresent()
            for (_, stat) in report.statistics {
                let values = stat.values
                guard stat.type == "inbound-rtp", values["kind"] as? String == "video" else { continue }
                let frames = self.number(values["framesDecoded"])
                let bytes = self.number(values["bytesReceived"])
                let decode = self.number(values["totalDecodeTime"])
                let delay = self.number(values["jitterBufferDelay"])
                let emitted = self.number(values["jitterBufferEmittedCount"])
                // Decode time and jitter buffer delay over the last interval;
                // lifetime averages keep the startup spike for a minute.
                let previous = self.lastInbound ?? (0, 0, 0, 0, 0, now)
                let dt = max(now - previous.at, 0.001)
                let decoded = frames - previous.frames
                let released = emitted - previous.jbEmitted
                if self.lastInbound != nil {
                    updated.decodeFps = decoded / dt
                    updated.bitrateMbps = (bytes - previous.bytes) * 8 / dt / 1_000_000
                }
                updated.decodeMs = decoded > 0 ? (decode - previous.decode) / decoded * 1000 : 0
                updated.jitterBufferMs = released > 0 ? (delay - previous.jbDelay) / released * 1000 : 0
                self.lastInbound = (frames, bytes, decode, delay, emitted, now)
                updated.jitterMs = self.number(values["jitter"]) * 1000
                updated.framesDropped = Int(self.number(values["framesDropped"]))
                updated.width = Int(self.number(values["frameWidth"]))
                updated.height = Int(self.number(values["frameHeight"]))
            }
            // RTT and network type of the pair the transport actually uses.
            if let pairId = report.statistics.values.first(where: { $0.type == "transport" })?
                .values["selectedCandidatePairId"] as? String,
               let pair = report.statistics[pairId]?.values {
                let rtt = self.number(pair["currentRoundTripTime"])
                if rtt > 0 { updated.rttMs = rtt * 1000 }
                if let localId = pair["localCandidateId"] as? String,
                   let network = report.statistics[localId]?.values["networkType"] as? String {
                    updated.network = network
                }
            }
            self.trials?.updateContext(String(
                format: "%dx%d rtt %.0f ms jb %.1f ms pres %.1f ms",
                updated.width, updated.height, updated.rttMs, updated.jitterBufferMs, updated.presentMs))
            self.statsTicks += 1
            if self.statsTicks % 5 == 0 {
                self.logNetwork(report)
                self.logAudio(report)
            }
            self.stats = updated
            DispatchQueue.main.async { [weak self] in self?.onStats?(updated) }
        }
    }

    /// Change of a cumulative stat since the previous call for `key`.
    private func delta(_ key: String, _ value: NSObject?) -> Double {
        let current = number(value)
        defer { lastCounters[key] = current }
        return current - (lastCounters[key] ?? current)
    }

    /// Where late frames come from, over the last 5 s: packet loss and
    /// retransmission, the sender's own cadence, or delivery to the decoder.
    private func logNetwork(_ report: RTCStatisticsReport) {
        let all = report.statistics
        guard let video = all.values.first(where: { $0.type == "inbound-rtp" && $0.values["kind"] as? String == "video" })?.values
        else { return }
        let assembled = delta("assembled", video["framesAssembledFromMultiplePackets"])
        let assembly = delta("assembly", video["totalAssemblyTime"])
        let arrival = decodeClock.snapshotArrival()
        let gaps = renderer.snapshotGaps()
        var path = "?"
        if let pairId = all.values.first(where: { $0.type == "transport" })?.values["selectedCandidatePairId"] as? String,
           let pair = all[pairId]?.values,
           let local = (pair["localCandidateId"] as? String).flatMap({ all[$0]?.values }),
           let remote = (pair["remoteCandidateId"] as? String).flatMap({ all[$0]?.values }) {
            // For a host candidate the local address tells the interface;
            // for a server-reflexive one it is the NAT mapping.
            path = "\(local["candidateType"] ?? "?" as NSString)→\(remote["candidateType"] ?? "?" as NSString) \(local["networkType"] ?? "?" as NSString) \(local["protocol"] ?? "?" as NSString) \(local["address"] ?? local["ip"] ?? "?" as NSString) → \(remote["address"] ?? remote["ip"] ?? "?" as NSString)"
        }
        if !loggedCandidates {
            // Once: how WebRTC ranks the local interfaces.
            loggedCandidates = true
            for candidate in all.values where candidate.type == "local-candidate" {
                let v = candidate.values
                LabLog.write("local candidate \(v["address"] ?? v["ip"] ?? "?" as NSString) \(v["networkType"] ?? "?" as NSString) \(v["candidateType"] ?? "?" as NSString) priority \(v["priority"] ?? 0 as NSNumber)")
            }
        }
        LabLog.write(String(
            format: "net 5s: lost %.0f nack %.0f retx %.0f pli %.0f freeze %.0f | assembly %.2f ms | frames %d late %d (max %.1f ms) bursts %d sender gaps %d | capture→decoder %@ ms | render gaps %d (max %.1f ms) | %@",
            delta("lost", video["packetsLost"]), delta("nack", video["nackCount"]),
            delta("retx", video["retransmittedPacketsReceived"]), delta("pli", video["pliCount"]),
            delta("freeze", video["freezeCount"]),
            assembled > 0 ? assembly / assembled * 1000 : 0,
            arrival.frames, arrival.late, arrival.maxLateMs, arrival.bursts, arrival.senderGaps,
            arrival.captureAgeMs.map { String(format: "%.1f", $0) } ?? "n/a",
            gaps.count, gaps.maxMs, path))
    }

    /// Audio health: samples the jitter buffer produced, how many were
    /// concealed, the output level (non-zero once playout pulls audio), and
    /// the audio jitter buffer delay over the last interval.
    private func logAudio(_ report: RTCStatisticsReport) {
        let all = report.statistics.values
        guard let inbound = all.first(where: { $0.type == "inbound-rtp" && $0.values["kind"] as? String == "audio" })?.values
        else { return }
        let level = String(format: "%.3f", number(inbound["audioLevel"]))
        let emitted = delta("audioEmitted", inbound["jitterBufferEmittedCount"])
        // Target includes A/V sync and playout-delay constraints; minimum is
        // what the network jitter alone needs.
        func perSample(_ key: String, _ stat: String) -> String {
            let value = delta(key, inbound[stat])
            return String(format: "%.1f", emitted > 0 ? value / emitted * 1000 : 0)
        }
        let jitterBuffer = perSample("audioDelay", "jitterBufferDelay")
        let target = perSample("audioTarget", "jitterBufferTargetDelay")
        let minimum = perSample("audioMinimum", "jitterBufferMinimumDelay")
        let packets = delta("audioPackets", inbound["packetsReceived"]) / 5
        LabLog.write("audio \(Int(packets)) pkt/s rx:\(Int(number(inbound["totalSamplesReceived"]))) concealed:\(Int(number(inbound["concealedSamples"]))) level:\(level) jb:\(jitterBuffer)ms target:\(target)ms minimum:\(minimum)ms")
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
