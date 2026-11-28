import SwiftUI
import UIKit

/// An element of the core's pad layout (`core/src/input/overlay.rs`), as far
/// as drawing it needs.
struct PadElement: Decodable, Equatable {
    let type: String
    let side: String?
    let target: String?
    let usage: UInt16?
    let label: String?
    let button: String?
    let x: Double
    let y: Double
    let size: Double

    /// What it is called on screen and for VoiceOver.
    var name: String {
        switch type {
        case "stick": side == "right" ? "Right stick" : "Left stick"
        case "dpad": "D-pad"
        case "trigger": side == "right" ? "RT" : "LT"
        case "key": label ?? "Key"
        case "mouse": "\(button ?? "Left") click"
        default: Self.targets[target ?? ""] ?? target ?? "Button"
        }
    }

    private static let targets = [
        "LeftBumper": "LB", "RightBumper": "RB", "LeftTrigger": "LT", "RightTrigger": "RT",
        "LeftThumb": "L3", "RightThumb": "R3", "DpadUp": "Up", "DpadDown": "Down",
        "DpadLeft": "Left", "DpadRight": "Right",
    ]
}

struct PadLayout: Decodable, Equatable {
    let opacity: Double
    let portrait: [PadElement]
    let landscape: [PadElement]

    func elements(portrait upright: Bool) -> [PadElement] { upright ? portrait : landscape }

    static func decode(_ json: String) -> PadLayout? {
        try? JSONDecoder().decode(PadLayout.self, from: Data(json.utf8))
    }

    /// Takes a string the core returned.
    static func take(_ raw: UnsafeMutablePointer<CChar>?) -> String {
        guard let raw else { return "" }
        defer { pw_free_string(raw) }
        return String(cString: raw)
    }

    static var defaultJSON: String { take(pw_overlay_default_layout()) }

    /// What an element's `size` is a fraction of on `surface`.
    static func unit(_ surface: CGSize) -> CGFloat {
        CGFloat(pw_overlay_unit(Float(surface.width), Float(surface.height)))
    }
}

/// Saved layouts: one for all games, and one of a game's own that wins for it.
struct PadLayoutStore {
    static let key = "touchPadLayout"
    var defaults: UserDefaults = .standard

    static func key(game: String?) -> String { game.map { "\(key).\($0)" } ?? key }

    func saved(game: String?) -> String? {
        game.flatMap { defaults.string(forKey: Self.key(game: $0)) } ?? defaults.string(forKey: Self.key)
    }

    func hasOwn(game: String) -> Bool { defaults.string(forKey: Self.key(game: game)) != nil }

    /// Saves `json` for `game` alone, or for every game without one of its own
    /// (`game` nil); `forget` drops that game's own layout, so the shared one applies to it.
    func save(_ json: String, game: String?, forget: String? = nil) {
        defaults.set(json, forKey: Self.key(game: game))
        if let forget { defaults.removeObject(forKey: Self.key(game: forget)) }
    }
}

/// Playwake's on-screen pad: the core works out what fingers on its layout
/// press (`pw_overlay_*`) and mixes it into the controller; this draws it,
/// passes the fingers on its elements in and taps haptic feedback.
@MainActor
final class TouchPad: ObservableObject {
    static let shared = TouchPad()

    @Published private(set) var layout: PadLayout
    /// The look of each element of the orientation in use, by index.
    @Published private(set) var looks: [pw_look] = []
    private var surface = CGSize.zero
    private var tracker = TouchTracker<ObjectIdentifier>()
    private let haptics = UIImpactFeedbackGenerator(style: .light)
    private let store = PadLayoutStore()

    private init() {
        layout = PadLayout.decode(PadLayout.defaultJSON)!
        load(game: nil)
    }

    /// Puts in use the layout `game` has of its own, else the one for all games.
    func load(game: String?) {
        let used = PadLayout.take(pw_overlay_set_layout(store.saved(game: game)))
        layout = PadLayout.decode(used) ?? layout
        tracker.reset()
        refresh()
    }

    var portrait: Bool { surface.height > surface.width }

    var elements: [PadElement] { layout.elements(portrait: portrait) }

    func layoutSurface(_ size: CGSize) {
        guard size != surface else { return }
        surface = size
        // Lets go of the fingers down, as the elements under them move.
        pw_overlay_surface(Float(size.width), Float(size.height))
        tracker.reset()
        refresh()
    }

    func hit(_ point: CGPoint) -> Bool {
        pw_overlay_hit(Float(point.x), Float(point.y)) >= 0
    }

    fileprivate func handle(_ touches: Set<UITouch>, phase: TouchPhase, in view: UIView) {
        var changed = false
        for touch in touches.sorted(by: { $0.timestamp < $1.timestamp }) {
            guard let call = tracker.call(for: ObjectIdentifier(touch), phase: phase, at: touch.location(in: view),
                                          direct: false, pressure: TouchInput.pressure(of: touch),
                                          now: touch.timestamp * 1000)
            else { continue }
            let result = pw_overlay_touch(call.id, call.phase.core, call.x, call.y, call.pressure)
            if result & UInt32(PW_OVERLAY_HAPTIC) != 0 { haptics.impactOccurred() }
            changed = true
        }
        guard changed else { return }
        Gamepad.shared.changed()
        refresh()
    }

    /// Lets go of every finger, as the pad hides; they stay out until they lift.
    func release() {
        pw_overlay_release()
        tracker.reset()
        Gamepad.shared.changed()
        refresh()
    }

    #if DEBUG
    /// Where a stick leans more than half way, for the input journal.
    private static func way(_ look: pw_look) -> String? {
        if abs(look.x) >= abs(look.y) {
            look.x > 0.5 ? "right" : look.x < -0.5 ? "left" : nil
        } else {
            look.y > 0.5 ? "down" : look.y < -0.5 ? "up" : nil
        }
    }
    #endif

    private func refresh() {
        let count = elements.count
        let next = (0..<count).map { index in
            var look = pw_look()
            pw_overlay_look(UInt32(index), &look)
            return look
        }
        #if DEBUG
        for (index, element) in elements.enumerated() {
            let old = index < looks.count ? looks[index] : pw_look()
            if old.pressed != next[index].pressed {
                InputJournal.shared.write("pad \(element.name) \(next[index].pressed ? "down" : "up")")
            }
            if element.type == "stick", let way = Self.way(next[index]), way != Self.way(old) {
                InputJournal.shared.write("pad \(element.name) leans \(way)")
            }
        }
        #endif
        looks = next
    }
}

/// Playwake's pad over the stream, in the safe area: drawn in SwiftUI, its
/// fingers taken by a view on top that lets the rest through to the stream.
struct TouchPadView: View {
    @ObservedObject private var pad = TouchPad.shared
    @ObservedObject private var virtualPad = VirtualPad.shared

    var body: some View {
        if virtualPad.customShown {
            GeometryReader { geometry in
                ZStack {
                    PadDrawing(elements: pad.layout.elements(portrait: geometry.size.height > geometry.size.width),
                               looks: pad.looks, surface: geometry.size)
                        .opacity(pad.layout.opacity)
                        .allowsHitTesting(false)
                    TouchPadSurface()
                }
            }
        }
    }
}

/// The elements at their places on `surface`.
struct PadDrawing: View {
    let elements: [PadElement]
    let looks: [pw_look]
    let surface: CGSize

    var body: some View {
        let unit = PadLayout.unit(surface)
        ZStack {
            ForEach(elements.indices, id: \.self) { index in
                let element = elements[index]
                PadElementView(element: element, look: index < looks.count ? looks[index] : pw_look(),
                               diameter: element.size * unit)
                    .position(x: element.x * surface.width, y: element.y * surface.height)
            }
        }
        .frame(width: surface.width, height: surface.height)
    }
}

struct PadElementView: View {
    let element: PadElement
    let look: pw_look
    let diameter: CGFloat

    var body: some View {
        shape
            .frame(width: diameter, height: diameter)
            .accessibilityElement()
            .accessibilityLabel(element.name)
            .accessibilityIdentifier("pad.\(element.name)")
            .accessibilityAddTraits(.isButton)
            .accessibilityValue(look.pressed ? "pressed" : "")
    }

    private var face: Color { look.pressed ? Color.white.opacity(0.85) : Color.black.opacity(0.35) }
    private var ink: Color { look.pressed ? .black : .white }
    private var rim: Color { .white.opacity(0.7) }

    @ViewBuilder private var shape: some View {
        switch element.type {
        case "stick":
            Circle().fill(Color.black.opacity(0.25))
                .overlay(Circle().stroke(rim, lineWidth: 2))
                .overlay(
                    Circle().fill(Color.white.opacity(look.pressed ? 0.9 : 0.6))
                        .frame(width: diameter * 0.45, height: diameter * 0.45)
                        .offset(x: CGFloat(look.x) * diameter * 0.275, y: CGFloat(look.y) * diameter * 0.275))
        case "dpad":
            DpadShape(look: look)
        case "trigger":
            RoundedRectangle(cornerRadius: diameter * 0.22, style: .continuous)
                .fill(Color.black.opacity(0.35))
                .overlay(alignment: .bottom) {
                    Rectangle().fill(Color.white.opacity(0.85))
                        .frame(height: diameter * CGFloat(look.pressed ? max(look.x, 0) : 0))
                }
                .clipShape(RoundedRectangle(cornerRadius: diameter * 0.22, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: diameter * 0.22, style: .continuous).stroke(rim, lineWidth: 2))
                .overlay(label.foregroundStyle(look.pressed ? .black : .white))
        case "key":
            RoundedRectangle(cornerRadius: diameter * 0.2, style: .continuous).fill(face)
                .overlay(RoundedRectangle(cornerRadius: diameter * 0.2, style: .continuous).stroke(rim, lineWidth: 2))
                .overlay(label.foregroundStyle(ink))
        default:
            Circle().fill(face)
                .overlay(Circle().stroke(tint ?? rim, lineWidth: 2))
                .overlay(label.foregroundStyle(look.pressed ? .black : (tint ?? .white)))
        }
    }

    /// The Xbox colours of the face buttons.
    private var tint: Color? {
        guard element.type == "button" else { return nil }
        switch element.target {
        case "A": return Color(hex: 0x6CC24A)
        case "B": return Color(hex: 0xE2483D)
        case "X": return Color(hex: 0x3A8FE8)
        case "Y": return Color(hex: 0xF2C230)
        default: return nil
        }
    }

    @ViewBuilder private var label: some View {
        let font = Font.system(size: max(10, diameter * 0.34), weight: .bold)
        switch (element.type, element.target) {
        case ("button", "View"): Image(systemName: "rectangle.on.rectangle").font(font)
        case ("button", "Menu"): Image(systemName: "line.3.horizontal").font(font)
        case ("button", "Guide"): Image(systemName: "logo.xbox").font(font)
        case ("mouse", _): Image(systemName: "computermouse").font(font)
        default: Text(element.name).font(font).minimumScaleFactor(0.5).lineLimit(1).padding(4)
        }
    }
}

/// A cross whose arms light up with the d-pad's direction.
private struct DpadShape: View {
    let look: pw_look

    var body: some View {
        GeometryReader { geometry in
            let side = min(geometry.size.width, geometry.size.height)
            let arm = side / 3
            ZStack {
                ForEach(Array(Self.arms.enumerated()), id: \.offset) { _, way in
                    let lit = look.pressed && (way.x != 0 && Float(way.x) == look.x || way.y != 0 && Float(way.y) == look.y)
                    RoundedRectangle(cornerRadius: arm * 0.25, style: .continuous)
                        .fill(lit ? Color.white.opacity(0.85) : Color.black.opacity(0.35))
                        .overlay(RoundedRectangle(cornerRadius: arm * 0.25, style: .continuous)
                            .stroke(Color.white.opacity(0.7), lineWidth: 2))
                        .frame(width: arm, height: arm)
                        .offset(x: CGFloat(way.x) * arm, y: CGFloat(way.y) * arm)
                }
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
        }
    }

    private static let arms: [(x: Int, y: Int)] = [(0, -1), (0, 1), (-1, 0), (1, 0)]
}

/// The view over the pad that takes the fingers landing on its elements.
struct TouchPadSurface: UIViewRepresentable {
    func makeUIView(context: Context) -> TouchPadSurfaceView { TouchPadSurfaceView() }
    func updateUIView(_ view: TouchPadSurfaceView, context: Context) {}
}

final class TouchPadSurfaceView: UIView {
    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override func layoutSubviews() {
        super.layoutSubviews()
        TouchPad.shared.layoutSurface(bounds.size)
    }

    /// Fingers on an element only: the rest go on to the stream below, and
    /// the mouse to `KeyboardMouse`.
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        if let event, !TouchSurfaceView.takes(event.type, buttons: event.buttonMask) { return nil }
        return TouchPad.shared.hit(point) ? self : nil
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchPad.shared.handle(touches, phase: .began, in: self)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchPad.shared.handle(touches, phase: .moved, in: self)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchPad.shared.handle(touches, phase: .ended, in: self)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        TouchPad.shared.handle(touches, phase: .cancelled, in: self)
    }
}
