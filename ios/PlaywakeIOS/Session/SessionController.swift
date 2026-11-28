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
        case pc(PcGame)
    }

    /// The PIN to enter in Sunshine on a PC being paired.
    struct Pairing: Equatable {
        let name: String
        let pin: String
    }

    /// Home navigation: tabs on iPhone, sidebar on iPad and Mac.
    enum Section: Hashable, CaseIterable {
        case consoles, cloud, pcs, settings
    }

    /// In the order the menu shows them.
    enum MenuItem: CaseIterable {
        case resume, guide, statistics, mouse, touch, keyboard, controls, reconnect, end

        /// The rows shown: Touch and Keyboard only while the stream takes
        /// touches, the on-screen pad's editor only with Playwake's pad.
        static func shown(touches: Bool, customPad: Bool = false) -> [MenuItem] {
            allCases.filter { (touches || ($0 != .touch && $0 != .keyboard)) && (customPad || $0 != .controls) }
        }
    }

    @Published private(set) var phase: Phase = .restoring
    @Published private(set) var device: DeviceCode?
    @Published private(set) var consoles: [XboxConsole] = []
    @Published private(set) var cloud: [CloudGame] = []
    @Published private(set) var pcs: [PcComputer] = []
    @Published private(set) var pcGames: [PcGame] = []
    @Published private(set) var pcLoading = true
    @Published private(set) var pairing: Pairing?
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
    /// The stream is from a PC: taps and the pointer over the video go to it.
    @Published private(set) var pcStream = false
    /// False until the first decoded frame; the connecting screen covers the video.
    @Published private(set) var firstFrame = false
    @Published private(set) var menuOpen = false
    /// The on-screen pad's editor is over the open menu.
    @Published var editingPad = false
    @Published var menuSelection = 0
    @Published var playoutMs: Int
    @Published var vsyncOn: Bool

    let renderer = LowLatencyVideoRenderer()
    private let core = PlaywakeCore.shared
    private let autoStream = ProcessInfo.processInfo.arguments.contains("-autostream")
    private var autoStarted = false
    private var rtc: XboxRTC?
    private var pcPlayer: PcPlayer?
    private var autoAdded = false
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
        core.pcCatalog()
        #if DEBUG
        let arguments = ProcessInfo.processInfo.arguments
        if let i = arguments.firstIndex(of: "-uiFixture"), i + 1 < arguments.count {
            loadFixture(arguments[i + 1])
        }
        // `-section cloud`: opens a section with live data.
        if let i = arguments.firstIndex(of: "-section"), i + 1 < arguments.count,
           let opened = Section.allCases.first(where: { "\($0)" == arguments[i + 1] }) {
            section = opened
        }
        #endif
    }

    func refreshPcs() {
        pcLoading = true
        core.pcCatalog()
    }

    /// Adds a PC by its address, for one the network search does not find.
    func addPc(address: String) {
        let address = address.trimmingCharacters(in: .whitespaces)
        guard !address.isEmpty else { return }
        pcLoading = true
        status = "Looking for Sunshine at \(address)…"
        core.addPc(address: address)
    }

    /// Pairs with an unpaired PC; `pairing` shows the PIN meanwhile.
    func pair(_ computer: PcComputer) {
        guard let address = computer.address else { return }
        status = "Pairing with \(computer.name)…"
        core.pairPc(address: address)
    }

    func startPc(_ game: PcGame) {
        beginStreaming(.pc(game), title: game.name, status: "Starting \(game.name) on the PC…")
        let player = PcPlayer(renderer: renderer)
        player.onFirstFrame = { [weak self] in self?.firstFrame = true }
        player.onMenu = { [weak self] events in self?.handleMenu(events) }
        pcPlayer = player
        let size = resolution(height)
        // A refused start says why in `pcError`; leave the connecting screen.
        if !player.start(game: game.id, width: size.0, height: size.1, fps: 60, bitrateKbps: 20_000) { stop() }
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
        beginStreaming(.game(game), title: game.name)
        core.streamCloud(game: game.id, height: height)
    }

    func stop() {
        rtc?.stop()
        rtc = nil
        pcPlayer?.stop()
        pcPlayer = nil
        core.stop()
        streaming = false
        firstFrame = false
        menuOpen = false
        editingPad = false
        UIApplication.shared.isIdleTimerDisabled = false
    }

    func reconnect() {
        guard let target = lastTarget else { return }
        stop()
        switch target {
        case .console(let console): startHome(console)
        case .game(let game): startCloud(game)
        case .pc(let game): startPc(game)
        }
    }

    // MARK: - Stream menu

    func openMenu() {
        if let pcPlayer { pcPlayer.openMenu() } else if let rtc { rtc.openMenu() } else if fixture { handleMenu(UInt32(PW_ROUTE_MENU_OPENED)) }
    }

    func closeMenu(guide: Bool = false) {
        if let pcPlayer { pcPlayer.closeMenu(guide: guide) } else if let rtc { rtc.closeMenu(guide: guide) } else if fixture { handleMenu(UInt32(PW_ROUTE_MENU_CLOSED)) }
    }

    /// Fingers on the video go to the stream, not to the menu: a PC stream
    /// on a touch screen.
    var streamTakesTouches: Bool { pcStream && TouchInput.supported }

    var menuItems: [MenuItem] {
        MenuItem.shown(touches: streamTakesTouches,
                       customPad: VirtualPad.supported && VirtualPad.Style.stored() == .custom)
    }

    func perform(_ item: MenuItem) {
        switch item {
        case .resume: closeMenu()
        case .guide: closeMenu(guide: true)
        case .statistics: showStats.toggle()
        case .mouse: KeyboardMouse.shared.toggleMode()
        case .touch: TouchInput.shared.toggleMode()
        case .keyboard:
            SoftKeyboard.shared.show()
            closeMenu()
        case .controls: editingPad = true
        case .reconnect: reconnect()
        case .end: stop()
        }
    }

    /// Whether the pad editor stays over the menu after route `events`:
    /// controller keys do not move the menu under it, Back closes it, and it
    /// goes with the menu.
    static func editorStays(after events: UInt32, menuOpen: Bool) -> Bool {
        menuOpen && events & UInt32(PW_ROUTE_KEY_BACK) == 0
    }

    private func handleMenu(_ events: UInt32) {
        if events & UInt32(PW_ROUTE_MENU_OPENED) != 0 {
            menuOpen = true
            menuSelection = 0
        }
        if events & UInt32(PW_ROUTE_MENU_CLOSED) != 0 {
            menuOpen = false
        }
        if editingPad {
            editingPad = Self.editorStays(after: events, menuOpen: menuOpen)
            return
        }
        let items = menuItems
        let count = items.count
        if events & UInt32(PW_ROUTE_KEY_PREVIOUS) != 0 {
            menuSelection = (menuSelection + count - 1) % count
        }
        if events & UInt32(PW_ROUTE_KEY_NEXT) != 0 {
            menuSelection = (menuSelection + 1) % count
        }
        if events & UInt32(PW_ROUTE_KEY_ACTIVATE) != 0 {
            perform(items[menuSelection])
        }
        if events & UInt32(PW_ROUTE_KEY_BACK) != 0 {
            closeMenu()
        }
    }

    private func beginStreaming(_ target: StreamTarget, title: String, status: String = "Starting the Xbox session…") {
        lastTarget = target
        if case .pc = target { pcStream = true } else { pcStream = false }
        streamTitle = title
        firstFrame = false
        menuOpen = false
        editingPad = false
        streaming = true
        streamStart = .now()
        streamStartedAt = CFAbsoluteTimeGetCurrent()
        stats = LabStats()
        self.status = status
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
        case .pcs(let computers, let games):
            pcs = computers
            pcGames = games
            pcLoading = false
            #if DEBUG
            autoPc()
            #endif
        case .pcPin(let pin, let name):
            pairing = Pairing(name: name, pin: pin)
            status = "Enter \(pin) in Sunshine on \(name)"
            LabLog.write("pc: pairing PIN \(pin) for \(name)")
        case .pcPaired(let name):
            pairing = nil
            status = "Paired with \(name)"
            refreshPcs()
        case .pcStreaming(let session, let hevc, let hdr):
            guard session == pcPlayer?.session else { break }
            status = "Streaming \(hevc ? "HEVC" : "H.264")\(hdr ? " HDR" : "")"
            LabLog.write("pc: \(status), host touch \(pw_input_host_supports_touch())")
        case .pcEnded(let session, let error):
            LabLog.write("pc: stream ended \(error ?? "")")
            // A stream that was replaced or stopped already is not this one.
            if session == pcPlayer?.session, streaming {
                stop()
                status = error ?? ""
            }
        case .pcError(let message):
            pairing = nil
            pcLoading = false
            status = message
            LabLog.write("pc: \(message)")
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
    /// `-pcPair <address>` pairs with that PC; `-pcPlay <name>` starts the
    /// first app whose name has it, for checks without touch or pointer.
    private func autoPc() {
        let arguments = ProcessInfo.processInfo.arguments
        func value(_ flag: String) -> String? {
            arguments.firstIndex(of: flag).flatMap { $0 + 1 < arguments.count ? arguments[$0 + 1] : nil }
        }
        if let address = value("-pcPair"), pairing == nil {
            if let computer = pcs.first(where: { $0.address == address && $0.state == .unpaired }) {
                pair(computer)
            } else if !pcs.contains(where: { $0.address == address }), !autoAdded {
                // The network search can miss it: add it by its address.
                autoAdded = true
                addPc(address: address)
            }
        }
        if let name = value("-pcPlay"), !autoStarted, !streaming,
           let game = pcGames.first(where: { $0.name.localizedCaseInsensitiveContains(name) }) {
            autoStarted = true
            startPc(game)
        }
    }

    /// `-uiFixture <state>`: fills the screens with sample data so every state
    /// can be checked without a console (welcome, signin, code, consoles,
    /// cloud, settings, connecting, stream, menu; pcstream, a PC's).
    func loadFixture(_ name: String) {
        fixture = true
        consoles = [
            XboxConsole(id: "1", name: "Living Room", model: "Xbox Series X", power: "On"),
            XboxConsole(id: "2", name: "Bedroom", model: "Xbox One X", power: "ConnectedStandby"),
        ]
        cloud = ["Forza Horizon 5", "Starfield", "Halo Infinite", "Sea of Thieves", "Hellblade II",
                 "Grounded", "Indiana Jones", "Microsoft Flight Simulator", "Gears 5", "Avowed",
                 "Doom: The Dark Ages", "Palworld"].sorted().enumerated().map { index, name in
            // Sample tiles without covers; the real fields come from the core.
            CloudGame(id: "\(index)", name: name, cover: nil, letter: String(name.prefix(1)),
                      monogram: name.split(separator: " ").prefix(2).compactMap(\.first).map(String.init).joined().uppercased(),
                      hue: Double(index * 47 % 360))
        }
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
        case "connecting", "stream", "menu", "pcstream":
            pcStream = name == "pcstream"
            streamTitle = pcStream ? "Gaming PC" : "Living Room"
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
