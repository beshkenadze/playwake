import Foundation
import UIKit

struct DeviceCode {
    let url: URL?
    let code: String
    let prefilled: Bool
}

@MainActor
final class SessionController: ObservableObject {
    enum Phase: Equatable {
        case restoring
        case signedOut
        case signingIn
        case ready
        case failed(String)
    }

    enum Target: Equatable {
        case home
        case cloud
    }

    @Published private(set) var phase: Phase = .restoring
    @Published private(set) var device: DeviceCode?
    @Published private(set) var consoles: [XboxConsole] = []
    @Published private(set) var cloud: [CloudGame] = []
    @Published private(set) var status = ""
    @Published private(set) var streaming = false
    @Published private(set) var stats = LabStats()
    @Published var target: Target = .home
    @Published var height: UInt32 = 720

    let renderer = LowLatencyVideoRenderer()
    private let core = PlaywakeCore.shared
    private var rtc: XboxRTC?
    private var streamStart: DispatchTime?
    private var accountActive = false

    init() {
        core.onEvent = { [weak self] event in
            self?.handle(event)
        }
    }

    func restore() {
        guard phase == .restoring || phase == .signedOut else { return }
        phase = .restoring
        core.restore()
    }

    func signIn() {
        phase = .signingIn
        status = "Contacting Microsoft…"
        core.signIn()
    }

    func cancelSignIn() {
        core.cancelSignIn()
        device = nil
        phase = .signedOut
        status = ""
    }

    func signOut() {
        stop()
        core.signOut()
    }

    func startHome(_ console: XboxConsole) {
        guard let index = consoles.firstIndex(of: console) else { return }
        beginStreaming()
        core.streamHome(console: index, height: height)
    }

    func startCloud(_ game: CloudGame) {
        guard let index = cloud.firstIndex(of: game) else { return }
        beginStreaming()
        core.streamCloud(game: index, height: height)
    }

    func stop() {
        rtc?.stop()
        rtc = nil
        core.stop()
        streaming = false
        UIApplication.shared.isIdleTimerDisabled = false
    }

    private func beginStreaming() {
        streaming = true
        streamStart = .now()
        stats = LabStats()
        status = "Starting the Xbox session…"
        UIApplication.shared.isIdleTimerDisabled = true
    }

    private func handle(_ event: CoreEvent) {
        switch event {
        case .device(let url, let code, let prefilled):
            device = DeviceCode(url: url, code: code, prefilled: prefilled)
            status = "Enter the code at \(url?.host ?? "microsoft.com/link")"
        case .account:
            accountActive = true
            phase = .ready
            device = nil
            status = "Loading consoles…"
            core.consoles()
            core.cloudCatalog()
        case .signedOut:
            accountActive = false
            phase = .signedOut
            consoles = []
            cloud = []
            status = ""
        case .error(let message):
            status = message
            if case .restoring = phase {
                phase = .signedOut
            }
            if case .signingIn = phase, device == nil {
                phase = .signedOut
            }
            if streaming {
                stop()
                phase = accountActive ? .ready : .signedOut
            }
        case .consoles(let list):
            consoles = list
            status = list.isEmpty ? "No consoles found on this account" : ""
        case .cloud(let games, _):
            // The Rust core emits a cached list first, then the fresh one.
            cloud = games
        case .status(let message):
            status = message
        case .ready(let stun, _):
            startMedia(stun: stun.first)
        case .done:
            device = nil
            if case .signingIn = phase {
                phase = .signedOut
            }
        }
    }

    private func startMedia(stun: String?) {
        guard streaming else { return }
        let rtc = XboxRTC(renderer: renderer)
        rtc.onStats = { [weak self] stats in self?.stats = stats }
        rtc.onEvent = { [weak self] event in
            guard let self else { return }
            switch event {
            case .state(let message):
                self.status = message
                LabLog.write("rtc: \(message)")
            case .inputReady:
                if let start = self.streamStart {
                    let ms = Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000
                    LabLog.write(String(format: "input ready after %.0f ms", ms))
                }
            case .inputSent:
                break
            }
        }
        self.rtc = rtc
        Gamepad.shared.start()
        let dimensions = resolution(height)
        rtc.start(.init(stun: stun, bitrateKbps: 20_000,
                        width: dimensions.0, height: dimensions.1))
    }

    private func resolution(_ height: UInt32) -> (UInt32, UInt32) {
        switch height {
        case 1080: return (1920, 1080)
        case 1440: return (2560, 1440)
        default: return (1280, 720)
        }
    }
}
