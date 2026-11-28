import Combine
import GameController
import SwiftUI
import UIKit

/// The on-screen keyboard for the stream: typed text goes as text
/// (`pw_input_text`), Backspace and Return as keys, and a bar over the
/// keyboard has the keys a phone keyboard lacks. Three fingers on the video
/// or the stream menu show it.
@MainActor
final class SoftKeyboard: ObservableObject {
    static let shared = SoftKeyboard()

    /// Asked for, and the stream takes input.
    @Published private(set) var visible = false
    @Published private(set) var keys = StickyKeys()

    private var wanted = false
    private var active = false

    /// Shows the keyboard now, or once the stream menu has closed.
    func show() {
        wanted = true
        update()
    }

    func hide() {
        wanted = false
        update()
    }

    func setStreamActive(_ active: Bool) {
        self.active = active
        // The core already let go of the latched modifiers it never got.
        if !active {
            wanted = false
            keys.clear()
        }
        update()
    }

    /// The user put the keyboard away.
    func dismissed() {
        guard wanted else { return }
        wanted = false
        update()
    }

    /// From the keyboard itself. A physical keyboard types through
    /// `KeyboardMouse`; its keys reaching this field too would type twice.
    func typed(_ text: String) {
        guard GCKeyboard.coalesced == nil else { return }
        send(keys.type(text))
    }

    func deleteBackward() {
        guard GCKeyboard.coalesced == nil else { return }
        send(keys.press(StickyKeys.backspace))
    }

    /// From the bar over the keyboard.
    func tap(_ modifier: StickyKeys.Modifier) {
        send(keys.tap(modifier))
    }

    func press(_ usage: UInt16) {
        send(keys.press(usage))
    }

    func controlAltDelete() {
        send(keys.chord([StickyKeys.Modifier.control.rawValue, StickyKeys.Modifier.alt.rawValue, StickyKeys.delete]))
    }

    private func update() {
        let visible = wanted && active
        if visible != self.visible { self.visible = visible }
    }

    private func send(_ outputs: [StickyKeys.Output]) {
        guard visible else { return }
        for output in outputs {
            switch output {
            case .key(let usage, let down): pw_input_key(KeyboardMouse.keyboardPage, usage, down)
            case .text(let text): pw_input_text(text)
            }
            #if DEBUG
            switch output {
            case .key(let usage, let down): InputJournal.shared.write(String(format: "key 0x%02X %@", usage, down ? "down" : "up"))
            case .text(let text): InputJournal.shared.write("text \(text)")
            }
            #endif
        }
    }
}

/// Ctrl, Alt and Win latch until the next key, which goes with them held.
/// Tapping a latched modifier again sends the latched ones alone (Win opens
/// Start).
struct StickyKeys: Equatable {
    /// HID usages of the left modifiers.
    enum Modifier: UInt16, CaseIterable {
        case control = 0xE0, alt = 0xE2, win = 0xE3
    }

    enum Output: Equatable {
        case key(UInt16, Bool)
        case text(String)
    }

    static let enter: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let delete: UInt16 = 0x4C
    static let leftShift: UInt16 = 0xE1
    static let f1: UInt16 = 0x3A
    static let right: UInt16 = 0x4F
    static let left: UInt16 = 0x50
    static let down: UInt16 = 0x51
    static let up: UInt16 = 0x52

    /// In the order tapped.
    private(set) var latched: [Modifier] = []

    mutating func tap(_ modifier: Modifier) -> [Output] {
        guard latched.contains(modifier) else {
            latched.append(modifier)
            return []
        }
        return chord([])
    }

    /// `usage` with the latched modifiers held around it.
    mutating func press(_ usage: UInt16, shift: Bool = false) -> [Output] {
        chord((shift ? [Self.leftShift] : []) + [usage])
    }

    /// The latched modifiers and then `usages` down, all up in reverse.
    mutating func chord(_ usages: [UInt16]) -> [Output] {
        var all = latched.map(\.rawValue)
        for usage in usages where !all.contains(usage) { all.append(usage) }
        latched = []
        return all.map { .key($0, true) } + all.reversed().map { .key($0, false) }
    }

    /// Text goes as text: it types what it says whatever the host's layout.
    /// Return is a key, and with modifiers latched a letter, digit or space
    /// is a key too, so Ctrl and C make Ctrl+C.
    mutating func type(_ text: String) -> [Output] {
        if text == "\n" { return press(Self.enter) }
        if !latched.isEmpty, let (usage, shift) = Self.usage(of: text) { return press(usage, shift: shift) }
        latched = []
        return [.text(text)]
    }

    mutating func clear() {
        latched = []
    }

    /// The HID usage of a US-layout letter, digit or space, and whether it needs Shift.
    static func usage(of text: String) -> (UInt16, Bool)? {
        // One scalar: `e` with a combining accent is one character, but not E.
        guard text.unicodeScalars.count == 1, let scalar = text.unicodeScalars.first, scalar.isASCII else { return nil }
        let value = UInt16(scalar.value)
        switch scalar {
        case "a"..."z": return (0x04 + value - 0x61, false)
        case "A"..."Z": return (0x04 + value - 0x41, true)
        case "1"..."9": return (0x1E + value - 0x31, false)
        case "0": return (0x27, false)
        case " ": return (space, false)
        default: return nil
        }
    }
}

/// The hidden field that holds the keyboard while it is shown.
struct SoftKeyboardField: UIViewRepresentable {
    @ObservedObject private var keyboard = SoftKeyboard.shared

    func makeUIView(context: Context) -> KeyCatcherView { KeyCatcherView() }

    func updateUIView(_ view: KeyCatcherView, context: Context) {
        view.scheduleFocus()
    }
}

final class KeyCatcherView: UIView, UIKeyInput {
    private lazy var bar = KeyBar()

    var autocapitalizationType = UITextAutocapitalizationType.none
    var autocorrectionType = UITextAutocorrectionType.no
    var spellCheckingType = UITextSpellCheckingType.no
    var smartQuotesType = UITextSmartQuotesType.no
    var smartDashesType = UITextSmartDashesType.no
    var smartInsertDeleteType = UITextSmartInsertDeleteType.no
    var keyboardAppearance = UIKeyboardAppearance.dark

    override var canBecomeFirstResponder: Bool { true }
    override var inputAccessoryView: UIView? { bar }

    /// Later, not while SwiftUI updates the view. The state is read when it
    /// runs, so an update queued before a newer one cannot undo it.
    func scheduleFocus() {
        DispatchQueue.main.async { [weak self] in self?.focus() }
    }

    /// Holds the keyboard while it is to be shown.
    func focus() {
        let visible = SoftKeyboard.shared.visible
        if visible, !isFirstResponder, window != nil { becomeFirstResponder() }
        if !visible, isFirstResponder { resignFirstResponder() }
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil { focus() }
    }

    /// Always, so Backspace reaches the host with nothing typed here.
    var hasText: Bool { true }

    func insertText(_ text: String) {
        SoftKeyboard.shared.typed(text)
    }

    func deleteBackward() {
        SoftKeyboard.shared.deleteBackward()
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { SoftKeyboard.shared.dismissed() }
        return resigned
    }
}

/// Esc, Tab, the modifiers, arrows, F1–F12 and Ctrl+Alt+Del over the keyboard.
private final class KeyBar: UIInputView {
    private var modifierButtons: [StickyKeys.Modifier: UIButton] = [:]
    private var subscription: AnyCancellable?

    init() {
        super.init(frame: CGRect(x: 0, y: 0, width: 0, height: 48), inputViewStyle: .keyboard)
        allowsSelfSizing = true

        let row = UIStackView()
        row.axis = .horizontal
        row.spacing = 6
        row.translatesAutoresizingMaskIntoConstraints = false

        let keyboard = SoftKeyboard.shared
        row.addArrangedSubview(key("Esc") { keyboard.press(StickyKeys.escape) })
        row.addArrangedSubview(key("Tab") { keyboard.press(StickyKeys.tab) })
        for (modifier, title) in [(StickyKeys.Modifier.control, "Ctrl"), (.alt, "Alt"), (.win, "Win")] {
            let button = key(title) { keyboard.tap(modifier) }
            modifierButtons[modifier] = button
            row.addArrangedSubview(button)
        }
        for (symbol, label, usage) in [("arrow.left", "Left", StickyKeys.left), ("arrow.up", "Up", StickyKeys.up),
                                       ("arrow.down", "Down", StickyKeys.down), ("arrow.right", "Right", StickyKeys.right)] {
            let button = key(symbol: symbol) { keyboard.press(usage) }
            button.accessibilityLabel = label
            row.addArrangedSubview(button)
        }
        for number in 1...12 {
            row.addArrangedSubview(key("F\(number)") { keyboard.press(StickyKeys.f1 + UInt16(number - 1)) })
        }
        row.addArrangedSubview(key("Ctrl+Alt+Del") { keyboard.controlAltDelete() })

        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(row)

        let close = key(symbol: "keyboard.chevron.compact.down") { keyboard.hide() }
        close.accessibilityLabel = "Hide keyboard"
        close.translatesAutoresizingMaskIntoConstraints = false

        addSubview(scroll)
        addSubview(close)
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 48),
            scroll.leadingAnchor.constraint(equalTo: safeAreaLayoutGuide.leadingAnchor, constant: 6),
            scroll.trailingAnchor.constraint(equalTo: close.leadingAnchor, constant: -6),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            close.trailingAnchor.constraint(equalTo: safeAreaLayoutGuide.trailingAnchor, constant: -6),
            close.centerYAnchor.constraint(equalTo: centerYAnchor),
            row.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            row.centerYAnchor.constraint(equalTo: scroll.frameLayoutGuide.centerYAnchor),
            row.heightAnchor.constraint(equalToConstant: 36),
        ])

        subscription = keyboard.$keys.sink { [weak self] keys in self?.show(latched: keys.latched) }
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    private func show(latched: [StickyKeys.Modifier]) {
        for (modifier, button) in modifierButtons {
            let on = latched.contains(modifier)
            button.configuration?.baseBackgroundColor = on ? .white : .secondarySystemFill
            button.configuration?.baseForegroundColor = on ? .black : .label
            button.isSelected = on
        }
    }

    private func key(_ title: String? = nil, symbol: String? = nil, action: @escaping () -> Void) -> UIButton {
        var configuration = UIButton.Configuration.filled()
        configuration.title = title
        configuration.image = symbol.flatMap { UIImage(systemName: $0) }
        configuration.baseBackgroundColor = .secondarySystemFill
        configuration.baseForegroundColor = .label
        configuration.cornerStyle = .medium
        configuration.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 12, bottom: 6, trailing: 12)
        configuration.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
            var attributes = attributes
            attributes.font = .systemFont(ofSize: 15, weight: .medium)
            return attributes
        }
        let button = UIButton(configuration: configuration, primaryAction: UIAction { _ in action() })
        button.setContentHuggingPriority(.required, for: .horizontal)
        button.setContentCompressionResistancePriority(.required, for: .horizontal)
        button.accessibilityLabel = title
        return button
    }
}
