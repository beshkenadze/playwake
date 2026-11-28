import SwiftUI

/// Moves and resizes the elements of Playwake's on-screen pad in the
/// orientation the device is held in, sets its opacity and resets it.
/// From a stream (`game`) the layout can be that game's own.
struct TouchPadEditor: View {
    let game: String?
    @Environment(\.dismiss) private var dismiss
    @State private var json: String
    @State private var layout: PadLayout
    @State private var selected: Int?
    @State private var drag: (index: Int, offset: CGSize)?
    @State private var forGame: Bool
    @State private var surface = CGSize.zero
    private let store: PadLayoutStore

    init(game: String?, store: PadLayoutStore = PadLayoutStore()) {
        self.game = game
        self.store = store
        let json = PadLayout.take(pw_overlay_normalize(store.saved(game: game)))
        _json = State(initialValue: json)
        _layout = State(initialValue: PadLayout.decode(json) ?? PadLayout.decode(PadLayout.defaultJSON)!)
        _forGame = State(initialValue: game.map(store.hasOwn) ?? false)
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            // Where the video sits during a stream.
            Rectangle().fill(Color(hex: 0x1C1F1E))
                .aspectRatio(KeyboardMouse.videoAspect, contentMode: .fit)
                .ignoresSafeArea()
            GeometryReader { geometry in
                elements(in: geometry.size)
                    .onAppear { surface = geometry.size }
                    .onChange(of: geometry.size) { _, size in
                        surface = size
                        selected = nil
                    }
            }
            panel
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }

    private var portrait: Bool { surface.height > surface.width }

    private var current: [PadElement] { layout.elements(portrait: portrait) }

    private func elements(in size: CGSize) -> some View {
        let unit = PadLayout.unit(size)
        let list = layout.elements(portrait: size.height > size.width)
        return ZStack {
            ForEach(list.indices, id: \.self) { index in
                let element = list[index]
                let offset = drag?.index == index ? drag!.offset : .zero
                PadElementView(element: element, look: pw_look(), diameter: element.size * unit)
                    .opacity(layout.opacity)
                    .overlay(Circle().stroke(Theme.mint, lineWidth: 3).opacity(selected == index ? 1 : 0))
                    .position(x: element.x * size.width + offset.width, y: element.y * size.height + offset.height)
                    .gesture(DragGesture(minimumDistance: 0)
                        .onChanged { value in
                            selected = index
                            drag = (index, value.translation)
                        }
                        .onEnded { value in
                            drag = nil
                            place(index, x: element.x + value.translation.width / size.width,
                                  y: element.y + value.translation.height / size.height, size: element.size)
                        })
            }
        }
        .frame(width: size.width, height: size.height)
    }

    private var panel: some View {
        VStack(spacing: 10) {
            if let selected, selected < current.count {
                let element = current[selected]
                HStack {
                    Text(element.name).frame(width: 80, alignment: .leading)
                    Slider(value: Binding(get: { element.size },
                                          set: { place(selected, x: element.x, y: element.y, size: $0) }),
                           in: 0.05...0.6)
                        .accessibilityIdentifier("padEditor.size")
                }
            } else {
                Text("Drag a control to move it; select one to resize it.")
                    .font(.footnote).foregroundStyle(Theme.muted)
            }
            HStack {
                Text("Opacity").frame(width: 80, alignment: .leading)
                Slider(value: Binding(get: { layout.opacity }, set: { setOpacity($0) }), in: 0.1...1)
                    .accessibilityIdentifier("padEditor.opacity")
            }
            if let game {
                Toggle("Only for \(game)", isOn: $forGame)
            }
            HStack(spacing: 10) {
                Button("Reset") { apply(PadLayout.defaultJSON) }
                    .buttonStyle(.playwakeSecondary)
                Button("Cancel") { dismiss() }
                    .buttonStyle(.playwakeSecondary)
                Button("Save") { save() }
                    .buttonStyle(.playwake)
            }
        }
        .font(.system(size: 15, weight: .medium))
        .padding(16)
        .frame(maxWidth: 360)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
        .padding(24)
    }

    private func apply(_ next: String) {
        guard let decoded = PadLayout.decode(next) else { return }
        json = next
        layout = decoded
    }

    private func place(_ index: Int, x: Double, y: Double, size: Double) {
        apply(PadLayout.take(pw_overlay_place(json, Float(surface.width), Float(surface.height), UInt32(index),
                                              Float(x), Float(y), Float(size))))
    }

    private func setOpacity(_ opacity: Double) {
        apply(PadLayout.take(pw_overlay_opacity(json, Float(opacity))))
    }

    private func save() {
        if let game, forGame {
            store.save(json, game: game)
        } else {
            store.save(json, game: nil, forget: game)
        }
        TouchPad.shared.load(game: game)
        dismiss()
    }
}
