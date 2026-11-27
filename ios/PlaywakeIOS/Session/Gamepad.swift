import Foundation
import GameController

/// Polls the first connected controller and maps it to the Xbox wire masks
/// used by the desktop evdev worker.
final class Gamepad {
    struct Snapshot {
        var buttons: UInt16 = 0
        var axes: [Int16] = [0, 0, 0, 0]
        var lt: UInt16 = 0
        var rt: UInt16 = 0
    }

    // Wire masks mirrored from media-worker/src/input.rs: X is 64 and Y 128,
    // so the top face button (Y) is 128 and the left one (X) is 64.
    enum Button {
        static let south: UInt16 = 16
        static let east: UInt16 = 32
        static let north: UInt16 = 128
        static let west: UInt16 = 64
        static let view: UInt16 = 8
        static let menu: UInt16 = 4
        static let guide: UInt16 = 2
        static let leftShoulder: UInt16 = 4096
        static let rightShoulder: UInt16 = 8192
        static let leftThumb: UInt16 = 16384
        static let rightThumb: UInt16 = 32768
        static let dpadUp: UInt16 = 256
        static let dpadDown: UInt16 = 512
        static let dpadLeft: UInt16 = 1024
        static let dpadRight: UInt16 = 2048
    }

    static let shared = Gamepad()

    private let lock = NSLock()
    private var current: GCController?
    private var changeHandler: (() -> Void)?
    /// Controller events arrive here instead of the main queue, which UI
    /// work can hold up.
    private let handlerQueue = DispatchQueue(label: "io.playwake.gamepad", qos: .userInteractive)

    /// Called on every controller input change, so input can be sent at once.
    var onChange: (() -> Void)? {
        get { lock.lock(); defer { lock.unlock() }; return changeHandler }
        set { lock.lock(); changeHandler = newValue; lock.unlock() }
    }
    private var observing = false
    private var discoveryTimer: DispatchSourceTimer?

    private init() {}

    func start() {
        lock.lock()
        let already = observing
        observing = true
        lock.unlock()
        guard !already else { return }

        NotificationCenter.default.addObserver(forName: .GCControllerDidConnect, object: nil, queue: .main) { [weak self] note in
            self?.adopt(note.object as? GCController)
        }
        NotificationCenter.default.addObserver(forName: .GCControllerDidDisconnect, object: nil, queue: .main) { [weak self] note in
            guard let controller = note.object as? GCController else { return }
            self?.lock.lock()
            if self?.current === controller { self?.current = nil }
            self?.lock.unlock()
        }
        #if os(macOS) || targetEnvironment(macCatalyst)
        // macOS only delivers controller input to the focused app by default.
        GCController.shouldMonitorBackgroundEvents = true
        #endif
        GCController.startWirelessControllerDiscovery(completionHandler: nil)
        let available = GCController.controllers()
        LabLog.write("gamepad scan: \(available.count) controller(s)")
        adopt(available.first)
        // The HID manager comes up asynchronously, and on macOS a pad already
        // connected is only exposed once the app is focused, so keep looking
        // until one shows up instead of guessing a timeout.
        guard discoveryTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + 1, repeating: 1.0)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.lock.lock()
            let adopted = self.current != nil
            self.lock.unlock()
            guard !adopted else {
                self.discoveryTimer?.cancel()
                self.discoveryTimer = nil
                return
            }
            let found = GCController.controllers()
            if !found.isEmpty {
                LabLog.write("gamepad retry: \(found.count) controller(s)")
                self.adopt(found.first)
            }
        }
        timer.resume()
        discoveryTimer = timer
    }

    private func adopt(_ controller: GCController?) {
        guard let controller else { return }
        controller.playerIndex = .index1
        controller.handlerQueue = handlerQueue
        controller.extendedGamepad?.valueChangedHandler = { [weak self] _, _ in self?.onChange?() }
        lock.lock()
        current = controller
        lock.unlock()
        LabLog.write("gamepad connected: \(controller.vendorName ?? "unknown")")
    }

    func snapshot() -> Snapshot {
        lock.lock()
        let controller = current
        lock.unlock()
        guard let pad = controller?.extendedGamepad else { return Snapshot() }

        var snap = Snapshot()
        if pad.buttonA.isPressed { snap.buttons |= Button.south }
        if pad.buttonB.isPressed { snap.buttons |= Button.east }
        if pad.buttonY.isPressed { snap.buttons |= Button.north }
        if pad.buttonX.isPressed { snap.buttons |= Button.west }
        if pad.leftShoulder.isPressed { snap.buttons |= Button.leftShoulder }
        if pad.rightShoulder.isPressed { snap.buttons |= Button.rightShoulder }
        if pad.buttonMenu.isPressed { snap.buttons |= Button.menu }
        if pad.buttonOptions?.isPressed == true { snap.buttons |= Button.view }
        if pad.buttonHome?.isPressed == true { snap.buttons |= Button.guide }
        if pad.leftThumbstickButton?.isPressed == true { snap.buttons |= Button.leftThumb }
        if pad.rightThumbstickButton?.isPressed == true { snap.buttons |= Button.rightThumb }
        if pad.dpad.up.isPressed { snap.buttons |= Button.dpadUp }
        if pad.dpad.down.isPressed { snap.buttons |= Button.dpadDown }
        if pad.dpad.left.isPressed { snap.buttons |= Button.dpadLeft }
        if pad.dpad.right.isPressed { snap.buttons |= Button.dpadRight }

        func axis(_ value: Float) -> Int16 {
            Int16(clamping: Int((max(-1, min(1, value)) * 32_767).rounded()))
        }
        // evdev reports Y down; the wire packet negates it, so feed the
        // negative of the controller's up-positive axis.
        snap.axes = [axis(pad.leftThumbstick.xAxis.value), axis(-pad.leftThumbstick.yAxis.value),
                     axis(pad.rightThumbstick.xAxis.value), axis(-pad.rightThumbstick.yAxis.value)]
        snap.lt = UInt16(clamping: Int((pad.leftTrigger.value * 65_535).rounded()))
        snap.rt = UInt16(clamping: Int((pad.rightTrigger.value * 65_535).rounded()))
        return snap
    }
}
