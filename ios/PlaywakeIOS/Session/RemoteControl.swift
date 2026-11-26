import Foundation
import QuartzCore

/// Debug remote control for unattended device tests (`-remote`).
///
/// Polls `Documents/remote/command.json` and acknowledges each applied command
/// by writing its id to `Documents/remote/done.json`:
///
///     {"id": 1, "press": ["south"], "ms": 150}        hold buttons
///     {"id": 2, "stick": [lx, ly, rx, ry], "ms": 400}  hold sticks, -1...1
///     {"id": 3, "snapshot": true}                     write `frame.jpg`
///     {"id": 4, "trials": true}                       run latency trials
///
/// Ids must grow; a command with an old id is ignored.
final class RemoteControl {
    private struct Command: Decodable {
        let id: Int
        var press: [String]?
        var stick: [Double]?
        var ms: Double?
        var snapshot: Bool?
        var trials: Bool?
    }

    private static let masks: [String: UInt16] = [
        "south": Gamepad.Button.south, "east": Gamepad.Button.east,
        "north": Gamepad.Button.north, "west": Gamepad.Button.west,
        "view": Gamepad.Button.view, "menu": Gamepad.Button.menu, "guide": Gamepad.Button.guide,
        "lb": Gamepad.Button.leftShoulder, "rb": Gamepad.Button.rightShoulder,
        "ls": Gamepad.Button.leftThumb, "rs": Gamepad.Button.rightThumb,
        "up": Gamepad.Button.dpadUp, "down": Gamepad.Button.dpadDown,
        "left": Gamepad.Button.dpadLeft, "right": Gamepad.Button.dpadRight,
    ]

    private let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("remote")
    private var commandURL: URL { directory.appendingPathComponent("command.json") }
    private let queue = DispatchQueue(label: "io.playwake.remote")
    private let lock = NSLock()
    private let renderer: LowLatencyVideoRenderer
    private let onInput: () -> Void
    private let onTrials: () -> Void
    private var timer: DispatchSourceTimer?
    private var lastId = 0
    private var hold: (buttons: UInt16, axes: [Int16]?, until: Double)?

    init(renderer: LowLatencyVideoRenderer, onInput: @escaping () -> Void, onTrials: @escaping () -> Void) {
        self.renderer = renderer
        self.onInput = onInput
        self.onTrials = onTrials
    }

    func start() {
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try? FileManager.default.removeItem(at: commandURL)
        renderer.keepLastFrame(true)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(100))
        timer.setEventHandler { [weak self] in self?.poll() }
        timer.resume()
        self.timer = timer
        LabLog.write("remote control on: \(directory.path)")
    }

    /// Cancels polling and waits for a poll already in flight.
    func stop() {
        timer?.cancel()
        timer = nil
        queue.sync {}
        renderer.keepLastFrame(false)
    }

    /// Applies held buttons and sticks to the polled pad state.
    func apply(to pad: inout Gamepad.Snapshot, at now: Double) {
        lock.lock(); defer { lock.unlock() }
        guard let hold else { return }
        guard now < hold.until else { self.hold = nil; return }
        pad.buttons |= hold.buttons
        if let axes = hold.axes { pad.axes = axes }
    }

    private func poll() {
        guard let data = try? Data(contentsOf: commandURL),
              let command = try? JSONDecoder().decode(Command.self, from: data),
              command.id > lastId else { return }
        lastId = command.id
        let until = CACurrentMediaTime() * 1000 + (command.ms ?? 150)
        if command.press != nil || command.stick != nil {
            let buttons = (command.press ?? []).reduce(UInt16(0)) { $0 | (Self.masks[$1] ?? 0) }
            let axes = command.stick.map { $0.prefix(4).map { Int16((min(max($0, -1), 1) * 32767).rounded()) } }
            lock.lock()
            hold = (buttons, axes.flatMap { $0.count == 4 ? $0 : nil }, until)
            lock.unlock()
            onInput()
        }
        var ok = true
        if command.snapshot == true {
            let frame = directory.appendingPathComponent("frame.jpg")
            try? FileManager.default.removeItem(at: frame)
            ok = renderer.writeSnapshot(to: frame)
        }
        if command.trials == true { onTrials() }
        try? Data("{\"id\":\(command.id),\"ok\":\(ok)}".utf8)
            .write(to: directory.appendingPathComponent("done.json"), options: .atomic)
        LabLog.write("remote \(command.id) applied")
    }
}
