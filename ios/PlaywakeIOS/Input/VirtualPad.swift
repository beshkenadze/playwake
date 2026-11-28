import GameController
import SwiftUI

/// The on-screen gamepad over a stream: the system one (`GCVirtualController`),
/// which appears to the app as an ordinary `GCController`, so `Gamepad` reads
/// it like a physical pad, or Playwake's own (`TouchPad`), which the core
/// works out and mixes into the pad.
@MainActor
final class VirtualPad: ObservableObject {
    enum Mode: String {
        case auto, always, never

        static let key = "virtualPad"

        static func stored(_ defaults: UserDefaults = .standard) -> Mode {
            defaults.string(forKey: key).flatMap(Mode.init(rawValue:)) ?? .auto
        }
    }

    enum Style: String {
        case custom, system

        static let key = "virtualPadStyle"

        static func stored(_ defaults: UserDefaults = .standard) -> Style {
            defaults.string(forKey: key).flatMap(Style.init(rawValue:)) ?? .custom
        }
    }

    /// Mac has no touch screen, so the setting is hidden there.
    #if targetEnvironment(macCatalyst)
    static let supported = false
    #else
    static let supported = true
    #endif

    static func shouldShow(mode: Mode, active: Bool, physicalPad: Bool) -> Bool {
        guard supported, active else { return false }
        switch mode {
        case .always: return true
        case .never: return false
        case .auto: return !physicalPad
        }
    }

    static let shared = VirtualPad()

    /// The system pad throws on a d-pad together with the left stick, and on
    /// Menu, Options and the stick buttons, so those are not here; View and
    /// Menu are `VirtualPadButtons`.
    private lazy var pad: GCVirtualController = {
        let configuration = GCVirtualController.Configuration()
        configuration.elements = [
            GCInputLeftThumbstick, GCInputRightThumbstick,
            GCInputButtonA, GCInputButtonB, GCInputButtonX, GCInputButtonY,
            GCInputLeftShoulder, GCInputRightShoulder, GCInputLeftTrigger, GCInputRightTrigger,
        ]
        return GCVirtualController(configuration: configuration)
    }()
    private var active = false
    @Published private(set) var connected = false
    /// Playwake's own pad is on screen.
    @Published private(set) var customShown = false
    private var connecting = false

    private init() {
        for name in [Notification.Name.GCControllerDidConnect, .GCControllerDidDisconnect] {
            NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { VirtualPad.shared.reconcile() }
            }
        }
    }

    /// Whether the stream is on screen and free for the pad (no menu over it).
    func setActive(_ active: Bool) {
        self.active = active
        reconcile()
    }

    private var physicalPad: Bool {
        GCController.controllers().contains { $0 !== pad.controller && $0.extendedGamepad != nil }
    }

    /// `connect` replies later, so one call is in flight at a time and the
    /// reply checks again against the state at that moment. Hiding
    /// disconnects rather than hides: `Gamepad` adopts a pad only when it
    /// connects, so showing again must connect again.
    private func reconcile() {
        guard Self.supported else { return }
        let show = Self.shouldShow(mode: .stored(), active: active, physicalPad: physicalPad)
        let custom = show && Style.stored() == .custom
        if custom != customShown {
            customShown = custom
            // Fingers on it when it hides stay out until they lift.
            if !custom { TouchPad.shared.release() }
        }
        reconcileSystem(show: show && !custom)
    }

    private func reconcileSystem(show: Bool) {
        guard !connecting else { return }
        if show && !connected {
            connecting = true
            pad.connect { error in
                Task { @MainActor in
                    let pad = VirtualPad.shared
                    pad.connecting = false
                    // Retried on the next controller or stream change, not in a loop.
                    if let error {
                        LabLog.write("virtual pad: \(error.localizedDescription)")
                        return
                    }
                    pad.connected = true
                    pad.reconcile()
                }
            }
        } else if !show && connected {
            pad.disconnect()
            connected = false
            Gamepad.shared.overlayButtons = 0
        }
    }
}

/// View and Menu, which the system pad cannot show, held down while touched.
struct VirtualPadButtons: View {
    @ObservedObject private var pad = VirtualPad.shared

    var body: some View {
        if pad.connected {
            HStack(spacing: 28) {
                PadButton(symbol: "rectangle.on.rectangle", label: "View", bit: Gamepad.Button.view)
                PadButton(symbol: "line.3.horizontal", label: "Menu", bit: Gamepad.Button.menu)
            }
            // The system pad fills the bottom corners, in portrait up to the
            // middle; the top edge is the statistics line.
            .padding(.top, 56)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
    }
}

private struct PadButton: View {
    let symbol: String
    let label: String
    let bit: UInt16
    /// Resets on release and also when the system cancels the touch.
    @GestureState private var pressed = false

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: 17, weight: .semibold))
            .foregroundStyle(.black)
            .frame(width: 44, height: 44)
            .background(Circle().fill(Color.white.opacity(pressed ? 1 : 0.75)))
            .contentShape(Circle())
            .gesture(DragGesture(minimumDistance: 0).updating($pressed) { _, state, _ in state = true })
            .onChange(of: pressed) { _, down in set(down) }
            .onDisappear { set(false) }
            .accessibilityLabel(label)
            .accessibilityAddTraits(.isButton)
    }

    private func set(_ down: Bool) {
        let gamepad = Gamepad.shared
        gamepad.overlayButtons = down ? gamepad.overlayButtons | bit : gamepad.overlayButtons & ~bit
    }
}
