import GameController
import ObjectiveC
import SwiftUI
import UIKit

/// A physical keyboard and mouse (Mac, iPad) for the stream. The core does
/// the work (`pw_input_*`); this reads `GCKeyboard` and `GCMouse`, captures
/// the pointer in game mode and follows it over the video in desktop mode.
@MainActor
final class KeyboardMouse: ObservableObject {
    enum Mode: String {
        /// The pointer is captured and the mouse sends relative motion.
        case game
        /// The pointer is free and its place over the video is sent.
        case desktop

        static let key = "mouseMode"

        static func stored(_ defaults: UserDefaults = .standard) -> Mode {
            defaults.string(forKey: key).flatMap(Mode.init(rawValue:)) ?? .game
        }

        var core: UInt32 { UInt32(self == .game ? PW_INPUT_GAME : PW_INPUT_DESKTOP) }
    }

    /// Every stream is 16:9 (`SessionController.resolution`).
    nonisolated static let videoAspect: CGFloat = 16.0 / 9.0
    nonisolated static let keyboardPage: UInt16 = 0x07
    nonisolated static let keyboard = UInt32(PW_INPUT_SOURCE_KEYBOARD)
    nonisolated static let mouse = UInt32(PW_INPUT_SOURCE_MOUSE)
    private nonisolated static let leftCommand: UInt16 = 0xE3
    private nonisolated static let rightCommand: UInt16 = 0xE7
    /// `GCMouse` scroll units in one wheel notch (Moonlight: ×20 of 120).
    private nonisolated static let scrollPerNotch: Float = 6

    static let shared = KeyboardMouse()

    @Published var mode: Mode {
        didSet {
            UserDefaults.standard.set(mode.rawValue, forKey: Mode.key)
            update()
        }
    }
    @Published private(set) var mouseConnected = false

    /// The stream shows video and its menu is closed.
    private var streamActive = false
    private var appActive = true
    /// Desktop mode: the left button as the view over the video saw it.
    private var leftHeld = false
    private let gate = InputGate()
    /// Where keyboard and mouse events arrive.
    let queue = DispatchQueue(label: "io.playwake.keyboard-mouse", qos: .userInteractive)

    private init() {
        mode = Mode.stored()
        appActive = UIApplication.shared.applicationState == .active
        pw_input_mode(mode.core)
        let center = NotificationCenter.default
        for name in [Notification.Name.GCKeyboardDidConnect, .GCMouseDidConnect] {
            center.addObserver(forName: name, object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { KeyboardMouse.shared.connect() }
            }
        }
        // A device that goes away mid-press never sends the release.
        for name in [Notification.Name.GCKeyboardDidDisconnect, .GCMouseDidDisconnect] {
            center.addObserver(forName: name, object: nil, queue: .main) { _ in
                MainActor.assumeIsolated {
                    // After the device's events already queued, so none of them presses again.
                    let gate = KeyboardMouse.shared.gate
                    KeyboardMouse.shared.queue.async { gate.locked { pw_input_release_all() } }
                    KeyboardMouse.shared.connect()
                }
            }
        }
        // Cmd+Tab and clicks on another app: let go of everything and free the pointer.
        for (name, active) in [(UIApplication.didBecomeActiveNotification, true),
                               (UIApplication.willResignActiveNotification, false)] {
            center.addObserver(forName: name, object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { KeyboardMouse.shared.setAppActive(active) }
            }
        }
        center.addObserver(forName: UIPointerLockState.didChangeNotification, object: nil, queue: .main) { note in
            let locked = (note.userInfo?[UIPointerLockState.sceneUserInfoKey] as? UIScene)?.pointerLockState?.isLocked
            LabLog.write("pointer lock \(locked.map { $0 ? "on" : "off" } ?? "unknown")")
        }
        connect()
    }

    /// Whether the stream takes keyboard and mouse: video up, menu closed.
    func setStreamActive(_ active: Bool) {
        streamActive = active
        update()
        #if DEBUG
        if active { runScript() }
        #endif
    }

    func toggleMode() {
        mode = mode == .game ? .desktop : .game
    }

    /// Desktop mode: the pointer at `point` in a view of `size` that shows
    /// the video aspect-fit, and the left button when `pressed` changes.
    func pointer(at point: CGPoint?, in size: CGSize, pressed: Bool? = nil) {
        let place = point.flatMap { Self.videoPosition(of: $0, in: size, aspect: Self.videoAspect) }
        let events = Self.pointerEvents(at: place, pressed: pressed, held: leftHeld)
        if case .left(let down) = events.last { leftHeld = down }
        gate.send {
            for event in events {
                switch event {
                case .position(let place): pw_input_mouse_position(Float(place.x), Float(place.y))
                case .left(let down): pw_input_mouse_button(Self.mouse, UInt32(PW_INPUT_BUTTON_LEFT), down)
                }
            }
        }
    }

    /// Lets go of the left button if the view still holds it.
    func pointerLifted() {
        pointer(at: nil, in: .zero, pressed: false)
    }

    enum PointerEvent: Equatable {
        case position(CGPoint)
        case left(Bool)
    }

    /// The position goes first, so a click lands where the pointer is: a
    /// separate button event could overtake the position.
    nonisolated static func pointerEvents(at place: CGPoint?, pressed: Bool?, held: Bool) -> [PointerEvent] {
        var events = place.map { [PointerEvent.position($0)] } ?? []
        // A press needs a place to land; a release goes regardless.
        if let pressed, pressed != held, place != nil || !pressed { events.append(.left(pressed)) }
        return events
    }

    /// `point` over an aspect-fit video of `aspect` (width / height) in a view
    /// of `size`, 0…1 from the video's top left; beyond 0…1 over the bars.
    nonisolated static func videoPosition(of point: CGPoint, in size: CGSize, aspect: CGFloat) -> CGPoint? {
        guard size.width > 0, size.height > 0, aspect > 0 else { return nil }
        let width = min(size.width, size.height * aspect)
        let height = width / aspect
        let origin = CGPoint(x: (size.width - width) / 2, y: (size.height - height) / 2)
        return CGPoint(x: (point.x - origin.x) / width, y: (point.y - origin.y) / height)
    }

    /// Command belongs to the Mac and iPad (Cmd+Tab, the app's shortcuts):
    /// it and the keys pressed with it stay here. Releases always go out,
    /// so nothing stays held on the host.
    nonisolated static func forwards(usage: UInt16, pressed: Bool, command: Bool) -> Bool {
        if usage == leftCommand || usage == rightCommand { return false }
        return !pressed || !command
    }

    private func setAppActive(_ active: Bool) {
        LabLog.write("app \(active ? "active" : "inactive")")
        appActive = active
        update()
    }

    private func update() {
        let open = streamActive && appActive
        let core = mode.core
        // Under the gate, so no event of the old mode slips in between.
        gate.set(open: open, desktop: mode == .desktop) { closing, modeChanged in
            if modeChanged { pw_input_mode(core) }
            if closing { pw_input_release_all() }
        }
        if !open || mode != .desktop { leftHeld = false }
        PointerLock.wanted = open && mode == .game && mouseConnected
    }

    private func connect() {
        if let keyboard = GCKeyboard.coalesced { configure(keyboard) }
        GCMouse.mice().forEach(configure)
        mouseConnected = !GCMouse.mice().isEmpty
        update()
    }

    private func configure(_ keyboard: GCKeyboard) {
        keyboard.handlerQueue = queue
        let gate = gate
        keyboard.keyboardInput?.keyChangedHandler = { input, _, code, pressed in
            let usage = UInt16(truncatingIfNeeded: code.rawValue)
            let command = input.button(forKeyCode: .leftGUI)?.isPressed == true
                || input.button(forKeyCode: .rightGUI)?.isPressed == true
            guard Self.forwards(usage: usage, pressed: pressed, command: command) else { return }
            gate.send { pw_input_key(Self.keyboard, Self.keyboardPage, usage, pressed) }
        }
    }

    private func configure(_ mouse: GCMouse) {
        mouse.handlerQueue = queue
        guard let input = mouse.mouseInput else { return }
        let gate = gate
        input.mouseMovedHandler = { _, dx, dy in
            // GameController reports y up; the core takes y down.
            gate.send { pw_input_mouse_move(dx, -dy) }
        }
        func bind(_ button: GCControllerButtonInput?, _ code: UInt32, inDesktop: Bool = true) {
            button?.pressedChangedHandler = { _, _, pressed in
                gate.send(inDesktop: inDesktop) { pw_input_mouse_button(Self.mouse, code, pressed) }
            }
        }
        // In desktop mode the view over the video sends the left button with its place.
        bind(input.leftButton, UInt32(PW_INPUT_BUTTON_LEFT), inDesktop: false)
        bind(input.rightButton, UInt32(PW_INPUT_BUTTON_RIGHT))
        bind(input.middleButton, UInt32(PW_INPUT_BUTTON_MIDDLE))
        let auxiliary = input.auxiliaryButtons ?? []
        bind(auxiliary.first, UInt32(PW_INPUT_BUTTON_X1))
        bind(auxiliary.dropFirst().first, UInt32(PW_INPUT_BUTTON_X2))
        // As Moonlight reads it on tvOS: up is positive, right is negative.
        input.scroll.valueChangedHandler = { _, x, y in
            gate.send { pw_input_scroll(-x / Self.scrollPerNotch, y / Self.scrollPerNotch) }
        }
    }
}

#if DEBUG
extension KeyboardMouse {
    /// `-inputScript "move 300 0; click; wait 800; text hi; key 0x29; snap /tmp/a.png"`
    /// drives the stream through the same C calls once it takes input, for
    /// checks without hardware events. Runs once per launch. Only the first
    /// `snap` of a launch shows live video; later ones repeat its frame.
    private static var scriptStarted = false

    fileprivate func runScript() {
        let arguments = ProcessInfo.processInfo.arguments
        guard !Self.scriptStarted, let index = arguments.firstIndex(of: "-inputScript"),
              index + 1 < arguments.count else { return }
        Self.scriptStarted = true
        let steps = arguments[index + 1].split(separator: ";")
            .map { $0.split(separator: " ").map(String.init) }
            .filter { !$0.isEmpty }
        DispatchQueue.global(qos: .userInitiated).async {
            Thread.sleep(forTimeInterval: 1)
            for step in steps {
                LabLog.write("input script: \(step.joined(separator: " "))")
                Self.perform(step)
            }
        }
    }

    private nonisolated static func perform(_ step: [String]) {
        func number(_ index: Int) -> Float {
            guard index < step.count else { return 0 }
            let text = step[index]
            return text.hasPrefix("0x") ? Float(UInt32(text.dropFirst(2), radix: 16) ?? 0) : Float(text) ?? 0
        }
        func press(_ usages: [UInt16]) {
            usages.forEach { pw_input_key(keyboard, keyboardPage, $0, true) }
            usleep(50_000)
            usages.reversed().forEach { pw_input_key(keyboard, keyboardPage, $0, false) }
        }
        switch step[0] {
        case "move": pw_input_mouse_move(number(1), number(2))
        case "pos": pw_input_mouse_position(number(1), number(2))
        case "click":
            let button = UInt32(step.count > 1 && step[1] == "right" ? PW_INPUT_BUTTON_RIGHT : PW_INPUT_BUTTON_LEFT)
            pw_input_mouse_button(mouse, button, true)
            usleep(50_000)
            pw_input_mouse_button(mouse, button, false)
        case "scroll": pw_input_scroll(0, number(1))
        case "text": pw_input_text(step.dropFirst().joined(separator: " "))
        case "key": press((1..<step.count).map { UInt16(number($0)) })
        case "wait": usleep(UInt32(number(1) * 1000))
        case "mode": DispatchQueue.main.sync { KeyboardMouse.shared.mode = step.count > 1 && step[1] == "desktop" ? .desktop : .game }
        case "snap": DispatchQueue.main.sync { snapshot(to: URL(fileURLWithPath: step.count > 1 ? step[1] : "/tmp/snap.png")) }
        default: LabLog.write("input script: unknown step \(step[0])")
        }
    }

    @MainActor
    private static func snapshot(to url: URL) {
        guard let window = UIApplication.shared.connectedScenes.compactMap({ ($0 as? UIWindowScene)?.keyWindow }).first
        else { return }
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        try? image.pngData()?.write(to: url)
    }
}
#endif

/// Whether input may reach the stream. A send runs under the lock, so once
/// the gate has closed no event read before it can still arrive.
final class InputGate: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    private var desktop = false

    /// Runs `body` while open; `inDesktop: false` skips it in desktop mode.
    func send(inDesktop: Bool = true, _ body: () -> Void) {
        lock.withLock { if open && (inDesktop || !desktop) { body() } }
    }

    /// Runs `body` under the lock, open or not.
    func locked(_ body: () -> Void) {
        lock.withLock(body)
    }

    /// `apply` runs under the lock, told whether an open gate closes and
    /// whether the mode changes.
    func set(open: Bool, desktop: Bool, apply: (_ closing: Bool, _ modeChanged: Bool) -> Void) {
        lock.withLock {
            apply(self.open && !open, self.desktop != desktop)
            self.open = open
            self.desktop = desktop
        }
    }
}

/// `prefersPointerLocked` for a SwiftUI window: its root is SwiftUI's own
/// hosting controller, so the getter is added to that class at run time.
@MainActor
private enum PointerLock {
    static var wanted = false {
        didSet { if wanted != oldValue { refresh() } }
    }
    private static var installed = Set<ObjectIdentifier>()

    private static func refresh() {
        let roots = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .compactMap(\.rootViewController)
        for root in roots {
            install(type(of: root))
            root.setNeedsUpdateOfPrefersPointerLocked()
        }
    }

    private static func install(_ type: AnyClass) {
        guard installed.insert(ObjectIdentifier(type)).inserted else { return }
        let getter: @convention(block) (AnyObject) -> Bool = { _ in
            MainActor.assumeIsolated { PointerLock.wanted }
        }
        let selector = #selector(getter: UIViewController.prefersPointerLocked)
        if !class_addMethod(type, selector, imp_implementationWithBlock(getter), "B@:") {
            LabLog.write("pointer lock: \(type) already overrides prefersPointerLocked")
        }
    }
}

/// Desktop mode: follows the pointer over the stream, also while a button
/// is held, and sends the left button with the pointer's place.
struct PointerArea: View {
    @GestureState private var pressing = false

    var body: some View {
        GeometryReader { geometry in
            Color.clear
                .contentShape(Rectangle())
                .onContinuousHover { phase in
                    if case .active(let point) = phase {
                        KeyboardMouse.shared.pointer(at: point, in: geometry.size)
                    }
                }
                .gesture(DragGesture(minimumDistance: 0)
                    .updating($pressing) { _, pressing, _ in pressing = true }
                    .onChanged { drag in KeyboardMouse.shared.pointer(at: drag.location, in: geometry.size, pressed: true) }
                    .onEnded { drag in KeyboardMouse.shared.pointer(at: drag.location, in: geometry.size, pressed: false) })
                // A cancelled drag never ends, but its gesture state resets.
                .onChange(of: pressing) { _, pressing in
                    if !pressing { KeyboardMouse.shared.pointerLifted() }
                }
                .onDisappear { KeyboardMouse.shared.pointerLifted() }
        }
    }
}
