import SwiftUI

/// The items that match `query`, best first, by the core's fuzzy search; all
/// of them in letter order for an empty query.
func searched<Item: LibraryItem>(_ all: [Item], _ query: String) -> [Item] {
    query.isEmpty ? all : PlaywakeCore.shared.search(all.map(\.name), query: query).map { all[$0] }
}

/// A scrolling grid of game covers with a letter rail beside it while the
/// whole catalog is on show.
struct LibraryGrid<Item: LibraryItem>: View {
    let games: [Item]
    let query: String
    let play: (Item) -> Void

    /// Each letter with the first game filed under it, in grid order.
    private var letters: [(letter: String, id: String)] {
        var seen = Set<String>()
        return games.compactMap { seen.insert($0.letter).inserted ? ($0.letter, $0.id) : nil }
    }

    var body: some View {
        ScrollViewReader { proxy in
            HStack(spacing: 0) {
                ScrollView {
                    if games.isEmpty {
                        Text("Nothing matches “\(query)”")
                            .foregroundStyle(Theme.muted)
                            .padding(.top, 80)
                    }
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 140), spacing: 16)],
                              alignment: .leading, spacing: 20) {
                        ForEach(games) { game in
                            Button { play(game) } label: { CoverTile(game: game) }
                                .buttonStyle(CoverButtonStyle())
                                .id(game.id)
                        }
                    }
                    .padding(.horizontal, 20)
                    .padding(.vertical, 12)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                let letters = letters
                if query.isEmpty, letters.count > 1 {
                    LetterRail(letters: letters.map(\.letter)) { index in
                        proxy.scrollTo(letters[index].id, anchor: .top)
                    }
                }
            }
        }
    }
}

/// A 2:3 cover with the game's name below it.
struct CoverTile<Item: LibraryItem>: View {
    let game: Item

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Color.clear
                .aspectRatio(2 / 3, contentMode: .fit)
                .overlay {
                    AsyncImage(url: game.cover(width: 360),
                               transaction: Transaction(animation: .easeOut(duration: 0.2))) { phase in
                        switch phase {
                        case .success(let image):
                            image.resizable().scaledToFill()
                        case .empty where game.hasCover:
                            CoverPlaceholder(monogram: game.monogram, hue: game.hue, loading: true)
                        default:
                            CoverPlaceholder(monogram: game.monogram, hue: game.hue, loading: false)
                        }
                    }
                }
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            Text(game.name)
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(Theme.muted)
                .lineLimit(1)
        }
    }
}

/// A tile for a game whose cover is missing or still on its way: a colour of
/// its own and its first letters, pulsing while the cover loads.
struct CoverPlaceholder: View {
    let monogram: String
    let hue: Double
    let loading: Bool

    var body: some View {
        // The SteamOS gradient, hsl(h, 45%, 36%) to hsl(h + 36°, 50%, 16%),
        // in SwiftUI's HSB.
        let tint = LinearGradient(
            colors: [Color(hue: hue / 360, saturation: 0.62, brightness: 0.52),
                     Color(hue: (hue + 36).truncatingRemainder(dividingBy: 360) / 360,
                           saturation: 0.67, brightness: 0.24)],
            startPoint: .top, endPoint: .bottom)
        let mark = Text(monogram)
            .font(.system(size: 40, weight: .bold))
            .foregroundStyle(.white.opacity(0.85))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(tint)
        if loading {
            mark.phaseAnimator([0.55, 1]) { view, opacity in view.opacity(opacity) }
                animation: { _ in .easeInOut(duration: 0.7) }
        } else {
            mark
        }
    }
}

/// A cover that lifts under the pointer and gets a mint frame.
struct CoverButtonStyle: ButtonStyle {
    @State private var hovered = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .overlay(alignment: .top) {
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .strokeBorder(Theme.mint, lineWidth: hovered ? 3 : 0)
                    .aspectRatio(2 / 3, contentMode: .fit)
                    .padding(-3)
            }
            .scaleEffect(configuration.isPressed ? 0.97 : hovered ? 1.03 : 1)
            .animation(.easeOut(duration: 0.15), value: hovered)
            .animation(.easeOut(duration: 0.1), value: configuration.isPressed)
            .onHover { hovered = $0 }
    }
}

/// The letters of the catalog, top to bottom: tap one or slide along them to
/// jump the grid to its first game. The rows shrink to fit a short window.
struct LetterRail: View {
    let letters: [String]
    let jump: (Int) -> Void
    /// The letter under the finger; a cancelled drag clears it too.
    @GestureState private var current: Int?

    var body: some View {
        GeometryReader { geometry in
            let row = min(18, geometry.size.height / CGFloat(max(letters.count, 1)))
            VStack(spacing: 0) {
                ForEach(letters.indices, id: \.self) { index in
                    Text(letters[index])
                        .font(.system(size: min(11, row * 0.65), weight: current == index ? .bold : .semibold))
                        .foregroundStyle(current == index ? Theme.onMint : Theme.muted)
                        .frame(width: 20, height: row)
                        .background(current == index ? Theme.mint : .clear, in: Capsule())
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0)
                .updating($current) { drag, current, _ in
                    let top = (geometry.size.height - CGFloat(letters.count) * row) / 2
                    let index = min(max(Int((drag.location.y - top) / row), 0), letters.count - 1)
                    if index != current {
                        current = index
                        jump(index)
                    }
                })
        }
        .frame(width: 28)
        .padding(.trailing, 6)
    }
}
