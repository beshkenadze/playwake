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
    private nonisolated static let leftCommand: UInt16 = 0xE3
    private nonisolated static let rightCommand: UInt16 = 0xE7
    /// `GCMouse` scroll units in one wheel notch (Moonlight: ×20 of 120).
    private nonisolated static let scrollPerNotch: Float = 6

    static let shared = KeyboardMouse()

    @Published var mode: Mode {
        didSet {
            UserDefaults.standard.set(mode.rawValue, forKey: Mode.key)
            pw_input_mode(mode.core)
            update()
        }
    }
    @Published private(set) var mouseConnected = false

    /// The stream shows video and its menu is closed.
    private var streamActive = false
    private var appActive = true
    private let gate = Gate()
    private let queue = DispatchQueue(label: "io.playwake.keyboard-mouse", qos: .userInteractive)

    private init() {
        mode = Mode.stored()
        appActive = UIApplication.shared.applicationState == .active
        pw_input_mode(mode.core)
        let center = NotificationCenter.default
        for name in [Notification.Name.GCKeyboardDidConnect, .GCMouseDidConnect, .GCMouseDidDisconnect] {
            center.addObserver(forName: name, object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { KeyboardMouse.shared.connect() }
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
    /// the video aspect-fit.
    nonisolated func pointer(at point: CGPoint, in size: CGSize) {
        guard gate.open, let place = Self.videoPosition(of: point, in: size, aspect: Self.videoAspect) else { return }
        pw_input_mouse_position(Float(place.x), Float(place.y))
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
        if gate.set(open), !open { pw_input_release_all() }
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
            guard gate.open else { return }
            let usage = UInt16(truncatingIfNeeded: code.rawValue)
            let command = input.button(forKeyCode: .leftGUI)?.isPressed == true
                || input.button(forKeyCode: .rightGUI)?.isPressed == true
            guard Self.forwards(usage: usage, pressed: pressed, command: command) else { return }
            pw_input_key(Self.keyboardPage, usage, pressed)
        }
    }

    private func configure(_ mouse: GCMouse) {
        mouse.handlerQueue = queue
        guard let input = mouse.mouseInput else { return }
        let gate = gate
        input.mouseMovedHandler = { _, dx, dy in
            // GameController reports y up; the core takes y down.
            if gate.open { pw_input_mouse_move(dx, -dy) }
        }
        func bind(_ button: GCControllerButtonInput?, _ code: UInt32) {
            button?.pressedChangedHandler = { _, _, pressed in
                if gate.open { pw_input_mouse_button(code, pressed) }
            }
        }
        bind(input.leftButton, UInt32(PW_INPUT_BUTTON_LEFT))
        bind(input.rightButton, UInt32(PW_INPUT_BUTTON_RIGHT))
        bind(input.middleButton, UInt32(PW_INPUT_BUTTON_MIDDLE))
        let auxiliary = input.auxiliaryButtons ?? []
        bind(auxiliary.first, UInt32(PW_INPUT_BUTTON_X1))
        bind(auxiliary.dropFirst().first, UInt32(PW_INPUT_BUTTON_X2))
        // As Moonlight reads it on tvOS: up is positive, right is negative.
        input.scroll.valueChangedHandler = { _, x, y in
            if gate.open { pw_input_scroll(-x / Self.scrollPerNotch, y / Self.scrollPerNotch) }
        }
    }
}

#if DEBUG
extension KeyboardMouse {
    /// `-inputScript "move 300 0; click; wait 800; text hi; key 0x29; snap /tmp/a.png"`
    /// drives the stream through the same C calls once it takes input, for
    /// checks without hardware events. Runs once per launch.
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
            usages.forEach { pw_input_key(keyboardPage, $0, true) }
            usleep(50_000)
            usages.reversed().forEach { pw_input_key(keyboardPage, $0, false) }
        }
        switch step[0] {
        case "move": pw_input_mouse_move(number(1), number(2))
        case "pos": pw_input_mouse_position(number(1), number(2))
        case "click":
            let button = UInt32(step.count > 1 && step[1] == "right" ? PW_INPUT_BUTTON_RIGHT : PW_INPUT_BUTTON_LEFT)
            pw_input_mouse_button(button, true)
            usleep(50_000)
            pw_input_mouse_button(button, false)
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

/// Whether input may reach the stream, read on the controller queue.
private final class Gate: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false

    var open: Bool { lock.withLock { value } }

    /// Returns whether the value changed.
    func set(_ open: Bool) -> Bool {
        lock.withLock {
            defer { value = open }
            return value != open
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

/// Follows the pointer over the stream in desktop mode, also while a button
/// is held.
struct PointerArea: View {
    var body: some View {
        GeometryReader { geometry in
            Color.clear
                .contentShape(Rectangle())
                .onContinuousHover { phase in
                    if case .active(let point) = phase {
                        KeyboardMouse.shared.pointer(at: point, in: geometry.size)
                    }
                }
                .gesture(DragGesture(minimumDistance: 0).onChanged { drag in
                    KeyboardMouse.shared.pointer(at: drag.location, in: geometry.size)
                })
        }
    }
}
