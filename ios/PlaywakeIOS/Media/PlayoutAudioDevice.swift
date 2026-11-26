import AudioToolbox
import AVFoundation
import CoreAudio
import WebRTC

/// Playout-only audio device for libwebrtc.
///
/// The default iOS ADM opens a VoiceProcessingIO unit, which always enables
/// the microphone: iOS kills the app without `NSMicrophoneUsageDescription`,
/// and voice processing adds latency. The stream is receive-only, so this
/// device drives a RemoteIO unit with input disabled and never records.
final class PlayoutAudioDevice: NSObject, RTCAudioDevice {
    private static let sampleRate = 48_000.0
    private static let channels = 2
    private static let ioBufferDuration = 0.005

    private var delegate: RTCAudioDeviceDelegate?
    private var unit: AudioUnit?
    /// Set before the unit starts and cleared after it stops; read on the
    /// render thread only while the unit runs.
    fileprivate var pull: RTCAudioDeviceGetPlayoutDataBlock?
    private(set) var isPlaying = false

    var deviceInputSampleRate: Double { Self.sampleRate }
    var inputIOBufferDuration: TimeInterval { Self.ioBufferDuration }
    var inputNumberOfChannels: Int { 1 }
    var inputLatency: TimeInterval { 0 }
    var deviceOutputSampleRate: Double { Self.sampleRate }
    var outputIOBufferDuration: TimeInterval { AVAudioSession.sharedInstance().ioBufferDuration }
    var outputNumberOfChannels: Int { Self.channels }
    var outputLatency: TimeInterval { AVAudioSession.sharedInstance().outputLatency }
    var isInitialized: Bool { delegate != nil }
    var isPlayoutInitialized: Bool { unit != nil }
    var isRecordingInitialized: Bool { false }
    var isRecording: Bool { false }

    func initialize(with delegate: RTCAudioDeviceDelegate) -> Bool {
        self.delegate = delegate
        return true
    }

    func terminateDevice() -> Bool {
        _ = stopPlayout()
        disposeUnit()
        delegate = nil
        return true
    }

    func initializePlayout() -> Bool {
        if unit != nil { return true }
        configureSession()
        guard let unit = Self.makeOutputUnit(refCon: Unmanaged.passUnretained(self).toOpaque()) else {
            LabLog.write("audio playout init failed")
            return false
        }
        self.unit = unit
        return true
    }

    func startPlayout() -> Bool {
        if isPlaying { return true }
        guard let unit, let delegate else { return false }
        pull = delegate.getPlayoutData
        guard AudioOutputUnitStart(unit) == noErr else {
            pull = nil
            return false
        }
        isPlaying = true
        let session = AVAudioSession.sharedInstance()
        LabLog.write("audio playout started rate:\(Int(session.sampleRate)) ch:\(Self.channels) io:\(Int(session.ioBufferDuration * 1_000_000))us")
        return true
    }

    func stopPlayout() -> Bool {
        if let unit, isPlaying { AudioOutputUnitStop(unit) }
        isPlaying = false
        pull = nil
        return true
    }

    func initializeRecording() -> Bool { false }
    func startRecording() -> Bool { false }
    func stopRecording() -> Bool { true }

    deinit { disposeUnit() }

    private func disposeUnit() {
        guard let unit else { return }
        AudioOutputUnitStop(unit)
        AudioUnitUninitialize(unit)
        AudioComponentInstanceDispose(unit)
        self.unit = nil
    }

    private func configureSession() {
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playback, mode: .moviePlayback)
            try session.setPreferredSampleRate(Self.sampleRate)
            try session.setPreferredIOBufferDuration(Self.ioBufferDuration)
            try session.setActive(true)
        } catch {
            LabLog.write("audio session: \(error.localizedDescription)")
        }
    }

    private static func makeOutputUnit(refCon: UnsafeMutableRawPointer) -> AudioUnit? {
        var description = AudioComponentDescription(
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_RemoteIO,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0)
        guard let component = AudioComponentFindNext(nil, &description) else { return nil }
        var created: AudioUnit?
        guard AudioComponentInstanceNew(component, &created) == noErr, let unit = created else { return nil }

        var inputOff: UInt32 = 0
        var format = AudioStreamBasicDescription(
            mSampleRate: sampleRate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kLinearPCMFormatFlagIsSignedInteger | kLinearPCMFormatFlagIsPacked,
            mBytesPerPacket: UInt32(2 * channels),
            mFramesPerPacket: 1,
            mBytesPerFrame: UInt32(2 * channels),
            mChannelsPerFrame: UInt32(channels),
            mBitsPerChannel: 16,
            mReserved: 0)
        var callback = AURenderCallbackStruct(inputProc: render, inputProcRefCon: refCon)
        let ok = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, 1,
                                      &inputOff, UInt32(MemoryLayout<UInt32>.size)) == noErr
            && AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0,
                                    &format, UInt32(MemoryLayout<AudioStreamBasicDescription>.size)) == noErr
            && AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0,
                                    &callback, UInt32(MemoryLayout<AURenderCallbackStruct>.size)) == noErr
            && AudioUnitInitialize(unit) == noErr
        guard ok else {
            AudioComponentInstanceDispose(unit)
            return nil
        }
        return unit
    }
}

/// Real-time render thread: no locks, logging or allocation.
private func render(
    refCon: UnsafeMutableRawPointer,
    flags: UnsafeMutablePointer<AudioUnitRenderActionFlags>,
    timestamp: UnsafePointer<AudioTimeStamp>,
    bus: UInt32,
    frames: UInt32,
    data: UnsafeMutablePointer<AudioBufferList>?
) -> OSStatus {
    guard let data else { return noErr }
    let device = Unmanaged<PlayoutAudioDevice>.fromOpaque(refCon).takeUnretainedValue()
    if let pull = device.pull {
        return pull(flags, timestamp, Int(bus), frames, data)
    }
    for buffer in UnsafeMutableAudioBufferListPointer(data) {
        if let bytes = buffer.mData { memset(bytes, 0, Int(buffer.mDataByteSize)) }
    }
    flags.pointee.insert(.unitRenderAction_OutputIsSilence)
    return noErr
}
