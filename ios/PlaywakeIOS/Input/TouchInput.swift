import SwiftUI
import UIKit

/// Fingers on the video for the stream. The core reads the gestures
/// (`pw_input_touch`): as a laptop trackpad, or touching the video where the
/// finger lands. A PC gets them as mouse or touch input, an Xbox as the
/// keyboard-and-mouse pad. While the stream takes touches, the menu opens
/// with its own button: a tap on the video clicks.
@MainActor
final class TouchInput: ObservableObject {
    enum Mode: String {
        case trackpad, direct

        static let key = "touchMode"

        static func stored(_ defaults: UserDefaults = .standard) -> Mode {
            defaults.string(forKey: key).flatMap(Mode.init(rawValue:)) ?? .trackpad
        }

        var core: UInt32 { UInt32(self == .trackpad ? PW_INPUT_TRACKPAD : PW_INPUT_DIRECT) }
    }

    /// Mac has no touch screen; its clicks are the mouse's (`KeyboardMouse`).
    #if targetEnvironment(macCatalyst)
    static let supported = false
    #else
    static let supported = true
    #endif

    static let shared = TouchInput()

    @Published var mode: Mode {
        didSet {
            UserDefaults.standard.set(mode.rawValue, forKey: Mode.key)
            // The core lets go of everything on a change; the fingers down stay out.
            tracker.reset()
            pw_input_mode(mode.core)
        }
    }

    private var active = false
    private var tracker = TouchTracker<ObjectIdentifier>()
    private var surface = CGSize.zero

    private init() {
        mode = Mode.stored()
        pw_input_mode(mode.core)
    }

    func toggleMode() {
        mode = mode == .trackpad ? .direct : .trackpad
    }

    /// Whether the stream takes touches: video up, menu closed.
    func setStreamActive(_ active: Bool) {
        self.active = active
        // The core already let go (`KeyboardMouse` releases all); fingers
        // still down stay out until they lift.
        if !active { tracker.reset() }
        SoftKeyboard.shared.setStreamActive(active)
    }

    /// The view the touches arrive in: trackpad distances are points over the video.
    func layout(_ size: CGSize) {
        let video = Self.videoSize(in: size)
        guard video != surface else { return }
        surface = video
        // Resets the gesture in progress, so only on a real change.
        pw_input_surface(Float(video.width), Float(video.height))
    }

    fileprivate func handle(_ touches: Set<UITouch>, phase: TouchPhase, in view: UIView) {
        guard active else { return }
        let size = view.bounds.size
        // Fingers and the pencil; a mouse or trackpad pointer is `KeyboardMouse`'s.
        for touch in touches.sorted(by: { $0.timestamp < $1.timestamp })
        where touch.type == .direct || touch.type == .pencil {
            guard let place = KeyboardMouse.videoPosition(of: touch.location(in: view), in: size,
                                                          aspect: KeyboardMouse.videoAspect),
                  let call = tracker.call(for: ObjectIdentifier(touch), phase: phase, at: place,
                                          direct: mode == .direct, pressure: Self.pressure(of: touch),
                                          now: touch.timestamp * 1000)
            else { continue }
            send(call)
        }
    }

    private func send(_ call: TouchCall) {
        let result = pw_input_touch(call.id, call.phase.core, call.x, call.y, call.pressure, call.now)
        #if DEBUG
        InputJournal.shared.write(String(format: "touch %@ %.2f %.2f", "\(call.phase)", call.x, call.y))
        #endif
        if result & UInt32(PW_INPUT_SHOW_KEYBOARD) != 0 { SoftKeyboard.shared.show() }
    }

    /// The aspect-fit video in a view of `size`.
    nonisolated static func videoSize(in size: CGSize) -> CGSize {
        let width = min(size.width, size.height * KeyboardMouse.videoAspect)
        return CGSize(width: width, height: width / KeyboardMouse.videoAspect)
    }

    /// 0…1; a finger without force sensing presses fully.
    private static func pressure(of touch: UITouch) -> Float {
        touch.maximumPossibleForce > 0 ? Float(touch.force / touch.maximumPossibleForce) : 1
    }
}

enum TouchPhase: CustomStringConvertible {
    case began, moved, ended, cancelled

    var core: UInt32 {
        switch self {
        case .began: UInt32(PW_INPUT_PHASE_BEGAN)
        case .moved: UInt32(PW_INPUT_PHASE_MOVED)
        case .ended: UInt32(PW_INPUT_PHASE_ENDED)
        case .cancelled: UInt32(PW_INPUT_PHASE_CANCELLED)
        }
    }

    var description: String {
        switch self {
        case .began: "began"
        case .moved: "moved"
        case .ended: "ended"
        case .cancelled: "cancelled"
        }
    }
}

struct TouchCall: Equatable {
    let id: UInt32
    let phase: TouchPhase
    let x: Float
    let y: Float
    let pressure: Float
    let now: Double
}

/// Gives each finger a stable id from its first touch to its lift and
/// turns its place into what `pw_input_touch` takes.
struct TouchTracker<Key: Hashable> {
    private var ids: [Key: UInt32] = [:]
    private var next: UInt32 = 0

    /// `place`: 0…1 over the video, beyond it over the bars. In direct mode a
    /// finger that lands on the bars stays out; one that slides onto them
    /// the core stops at the video's edge. `nil` for a finger not followed.
    mutating func call(for key: Key, phase: TouchPhase, at place: CGPoint, direct: Bool,
                       pressure: Float, now: Double) -> TouchCall? {
        let id: UInt32
        switch phase {
        case .began:
            guard ids[key] == nil, !direct || (0...1).contains(place.x) && (0...1).contains(place.y) else { return nil }
            id = next
            next &+= 1
            ids[key] = id
        case .moved:
            guard let known = ids[key] else { return nil }
            id = known
        case .ended, .cancelled:
            guard let known = ids.removeValue(forKey: key) else { return nil }
            id = known
        }
        return TouchCall(id: id, phase: phase, x: Float(place.x), y: Float(place.y), pressure: pressure, now: now)
    }

    /// Forgets the fingers down: the rest of their touches stay out.
    mutating func reset() {
        ids.removeAll()
    }
}

/// The view over the video that takes fingers for the stream.
struct TouchSurface: UIViewRepresentable {
    func makeUIView(context: Context) -> TouchSurfaceView { TouchSurfaceView() }
    func updateUIView(_ view: TouchSurfaceView, context: Context) {}
}

final class TouchSurfaceView: UIView {
    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
        isAccessibilityElement = true
        accessibilityIdentifier = "touchSurface"
        accessibilityLabel = "Stream"
        // VoiceOver lets the fingers through to the stream.
        accessibilityTraits = .allowsDirectInteraction
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override func layoutSubviews() {
        super.layoutSubviews()
        TouchInput.shared.layout(bounds.size)
    }

    /// Fingers only: the mouse's hover, clicks and scrolls go on to the
    /// views below and to `KeyboardMouse`.
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        if let event, !Self.takes(event.type, buttons: event.buttonMask) { return nil }
        return super.hitTest(point, with: event)
    }

    nonisolated static func takes(_ type: UIEvent.EventType, buttons: UIEvent.ButtonMask) -> Bool {
        type == .touches && buttons.isEmpty
    }

    /// The video, for VoiceOver and UI tests, not the bars beside it.
    override var accessibilityFrame: CGRect {
        get {
            let video = TouchInput.videoSize(in: bounds.size)
            let frame = CGRect(x: (bounds.width - video.width) / 2, y: (bounds.height - video.height) / 2,
                               width: video.width, height: video.height)
            return UIAccessibility.convertToScreenCoordinates(frame, in: self)
        }
        set {}
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchInput.shared.handle(touches, phase: .began, in: self)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchInput.shared.handle(touches, phase: .moved, in: self)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchInput.shared.handle(touches, phase: .ended, in: self)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchInput.shared.handle(touches, phase: .cancelled, in: self)
    }
}

/// Opens the stream menu while taps on the video belong to the stream.
struct StreamMenuButton: View {
    let action: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Button(action: action) {
                Image(systemName: "ellipsis")
                    .font(.system(size: 16, weight: .bold))
                    .foregroundStyle(.white)
                    .frame(width: 36, height: 36)
                    .background(Circle().fill(Color.black.opacity(0.45)))
                    .frame(width: 44, height: 44)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Stream menu")
            .accessibilityIdentifier("streamMenuButton")
            #if DEBUG
            InputJournalView()
            #endif
        }
        .padding(.leading, 12)
        .padding(.top, 8)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

#if DEBUG
/// `-inputJournal`: the last touch and keyboard calls on screen, for UI
/// tests, which cannot see where the core sends them.
@MainActor
final class InputJournal: ObservableObject {
    static let shared = InputJournal()
    static let enabled = ProcessInfo.processInfo.arguments.contains("-inputJournal")

    @Published private(set) var lines: [String] = []

    func write(_ line: String) {
        guard Self.enabled else { return }
        lines.append(line)
        if lines.count > 40 { lines.removeFirst(lines.count - 40) }
    }
}

private struct InputJournalView: View {
    @ObservedObject private var journal = InputJournal.shared

    var body: some View {
        if InputJournal.enabled {
            Text(journal.lines.suffix(6).joined(separator: "\n"))
                .font(.system(size: 9).monospaced())
                .foregroundStyle(.white.opacity(0.7))
                .allowsHitTesting(false)
                .accessibilityLabel(journal.lines.joined(separator: "\n"))
                .accessibilityIdentifier("inputJournal")
        }
    }
}
#endif
