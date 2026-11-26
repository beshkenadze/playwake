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

    /// What the last stream played, for Reconnect.
    enum StreamTarget: Equatable {
        case console(XboxConsole)
        case game(CloudGame)
    }

    /// Home navigation: tabs on iPhone, sidebar on iPad and Mac.
    enum Section: Hashable, CaseIterable {
        case consoles, cloud, settings
    }

    enum MenuItem: CaseIterable {
        case resume, guide, statistics, reconnect, end
    }

    @Published private(set) var phase: Phase = .restoring
    @Published private(set) var device: DeviceCode?
    @Published private(set) var consoles: [XboxConsole] = []
    @Published private(set) var cloud: [CloudGame] = []
    @Published private(set) var status = ""
    @Published private(set) var streaming = false
    @Published private(set) var stats = LabStats()
    @Published var target: Target = .home
    @Published var section: Section = .consoles
    @Published var height: UInt32 = 720 {
        didSet { UserDefaults.standard.set(Int(height), forKey: "streamHeight") }
    }
    @Published var showStats: Bool {
        didSet { if !fixture { UserDefaults.standard.set(showStats, forKey: "showStats") } }
    }
    @Published private(set) var streamTitle = ""
    /// False until the first decoded frame; the connecting screen covers the video.
    @Published private(set) var firstFrame = false
    @Published private(set) var menuOpen = false
    @Published var menuSelection = 0
    @Published var playoutMs: Int
    @Published var vsyncOn: Bool

    let renderer = LowLatencyVideoRenderer()
    private let core = PlaywakeCore.shared
    private let autoStream = ProcessInfo.processInfo.arguments.contains("-autostream")
    private var autoStarted = false
    private var rtc: XboxRTC?
    private var streamStart: DispatchTime?
    private var accountActive = false
    private var lastTarget: StreamTarget?
    private var fixture = false
    private var streamStartedAt: CFAbsoluteTime = 0

    init() {
        let defaults = UserDefaults.standard
        playoutMs = WebRTCTrials.storedPlayoutMs
        showStats = defaults.bool(forKey: "showStats")
        vsyncOn = defaults.object(forKey: "vsyncOn") == nil
            ? true : defaults.bool(forKey: "vsyncOn")
        let requested = defaults.integer(forKey: "streamHeight")
        if [720, 1080, 1440].contains(requested) {
            height = UInt32(requested)
        }
        core.onEvent = { [weak self] event in
            self?.handle(event)
        }
        #if DEBUG
        let arguments = ProcessInfo.processInfo.arguments
        if let i = arguments.firstIndex(of: "-uiFixture"), i + 1 < arguments.count {
            loadFixture(arguments[i + 1])
        }
        #endif
    }

    func refreshConsoles() {
        status = "Loading consoles…"
        core.consoles()
        core.cloudCatalog()
    }

    /// Field trials are read once per process, so a new cushion applies after
    /// the app is relaunched; stop the stream so the change is not mistaken
    /// for a live tweak.
    func setPlayoutMs(_ value: Int) {
        playoutMs = value
        UserDefaults.standard.set(value, forKey: "playoutMs")
        if streaming {
            stop()
            status = "Smoothing \(value) ms saved — relaunch Playwake to apply"
        }
    }

    /// Applies live: CAMetalLayer.displaySyncEnabled can change mid-stream.
    func setVsync(_ on: Bool) {
        vsyncOn = on
        UserDefaults.standard.set(on, forKey: "vsyncOn")
        renderer.setDisplaySync(on)
    }

    func restore() {
        guard phase == .restoring || phase == .signedOut, !fixture else { return }
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
        // By id: a refresh may have changed the console's power state.
        guard let index = consoles.firstIndex(where: { $0.id == console.id }) else {
            status = "\(console.name) is no longer on this account"
            return
        }
        beginStreaming(.console(console), title: console.name.isEmpty ? console.model : console.name)
        core.streamHome(console: index, height: height)
    }

    func startCloud(_ game: CloudGame) {
        guard let index = cloud.firstIndex(where: { $0.id == game.id }) else {
            status = "\(game.name) is no longer in the catalog"
            return
        }
        beginStreaming(.game(game), title: game.name)
        core.streamCloud(game: index, height: height)
    }

    func stop() {
        rtc?.stop()
        rtc = nil
        core.stop()
        streaming = false
        firstFrame = false
        menuOpen = false
        UIApplication.shared.isIdleTimerDisabled = false
    }

    func reconnect() {
        guard let target = lastTarget else { return }
        stop()
        switch target {
        case .console(let console): startHome(console)
        case .game(let game): startCloud(game)
        }
    }

    // MARK: - Stream menu

    func openMenu() {
        if let rtc { rtc.openMenu() } else if fixture { handleMenu(UInt32(PW_ROUTE_MENU_OPENED)) }
    }

    func closeMenu(guide: Bool = false) {
        if let rtc { rtc.closeMenu(guide: guide) } else if fixture { handleMenu(UInt32(PW_ROUTE_MENU_CLOSED)) }
    }

    func perform(_ item: MenuItem) {
        switch item {
        case .resume: closeMenu()
        case .guide: closeMenu(guide: true)
        case .statistics: showStats.toggle()
        case .reconnect: reconnect()
        case .end: stop()
        }
    }

    private func handleMenu(_ events: UInt32) {
        if events & UInt32(PW_ROUTE_MENU_OPENED) != 0 {
            menuOpen = true
            menuSelection = 0
        }
        if events & UInt32(PW_ROUTE_MENU_CLOSED) != 0 {
            menuOpen = false
        }
        let count = MenuItem.allCases.count
        if events & UInt32(PW_ROUTE_KEY_PREVIOUS) != 0 {
            menuSelection = (menuSelection + count - 1) % count
        }
        if events & UInt32(PW_ROUTE_KEY_NEXT) != 0 {
            menuSelection = (menuSelection + 1) % count
        }
        if events & UInt32(PW_ROUTE_KEY_ACTIVATE) != 0 {
            perform(MenuItem.allCases[menuSelection])
        }
        if events & UInt32(PW_ROUTE_KEY_BACK) != 0 {
            closeMenu()
        }
    }

    private func beginStreaming(_ target: StreamTarget, title: String) {
        lastTarget = target
        streamTitle = title
        firstFrame = false
        menuOpen = false
        streaming = true
        streamStart = .now()
        streamStartedAt = CFAbsoluteTimeGetCurrent()
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
            if autoStream, !autoStarted, !streaming, let console = list.first {
                autoStarted = true
                startHome(console)
            }
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
        rtc.onStats = { [weak self] stats in
            guard let self else { return }
            self.stats = stats
            // The renderer outlives sessions, so its last frame must be from this one.
            if !self.firstFrame, stats.decodeFps > 0 || self.renderer.lastFrameAt > self.streamStartedAt {
                self.firstFrame = true
            }
            LabLog.write("stats \(stats.summary.replacingOccurrences(of: "\n", with: " | "))")
            SessionEvents.shared.publish(stats)
        }
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
            case .menu(let events):
                self.handleMenu(events)
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

    #if DEBUG
    /// `-uiFixture <state>`: fills the screens with sample data so every state
    /// can be checked without a console (welcome, signin, code, consoles,
    /// cloud, settings, connecting, stream, menu).
    func loadFixture(_ name: String) {
        fixture = true
        consoles = [
            XboxConsole(id: "1", name: "Living Room", model: "Xbox Series X", power: "On"),
            XboxConsole(id: "2", name: "Bedroom", model: "Xbox One X", power: "ConnectedStandby"),
        ]
        cloud = ["Forza Horizon 5", "Starfield", "Halo Infinite", "Sea of Thieves", "Hellblade II",
                 "Grounded", "Indiana Jones", "Microsoft Flight Simulator", "Gears 5", "Avowed",
                 "Doom: The Dark Ages", "Palworld"].enumerated().map { CloudGame(id: "\($0.offset)", name: $0.element) }
        phase = .ready
        status = ""
        switch name {
        case "welcome": phase = .signedOut
        case "signin": phase = .signingIn
        case "code":
            phase = .signingIn
            device = DeviceCode(url: URL(string: "https://www.microsoft.com/link?otc=K7PMQ2XD"),
                                code: "K7PM-Q2XD", prefilled: true)
        case "cloud": section = .cloud
        case "settings": section = .settings
        case "connecting", "stream", "menu":
            streamTitle = "Living Room"
            streaming = true
            status = "Starting the Xbox session…"
            firstFrame = name != "connecting"
            showStats = true
            menuOpen = name == "menu"
            stats.width = 1280
            stats.height = 720
            stats.decodeFps = 60
            stats.rttMs = 3
            stats.network = "wifi"
            stats.decodeMs = 3.9
            stats.submitMs = 1.5
            stats.presentMs = 13.4
            stats.jitterBufferMs = 0.6
        default: break
        }
    }
    #endif
}
