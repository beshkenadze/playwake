import SwiftUI

/// Playwake palette, shared with the SteamOS client (src/deck.rs) and the
/// Claude Design mockups.
enum Theme {
    static let canvas = Color(hex: 0x161818)
    static let panel = Color(hex: 0x1F1F21)
    static let card = Color(hex: 0x2C2C2E)
    static let text = Color(hex: 0xECECEC)
    static let muted = Color(hex: 0xA1A1A6)
    static let mint = Color(hex: 0x8DDCB3)
    static let deep = Color(hex: 0x2A7A58)
    static let onMint = Color(hex: 0x062A1F)
    static let amber = Color(hex: 0xE3B35C)
    static let danger = Color(hex: 0xFF6B6B)

    /// Background of the welcome and connecting screens: the hero's green glow.
    static var glow: some View {
        ZStack {
            LinearGradient(colors: [Color(hex: 0x0E1D1A), Color(hex: 0x0A1312), Color(hex: 0x090E0D)],
                           startPoint: .top, endPoint: .bottom)
            RadialGradient(colors: [Color(hex: 0x22E3B4).opacity(0.42), .clear],
                           center: UnitPoint(x: 1.05, y: 0.3), startRadius: 0, endRadius: 420)
        }
        .ignoresSafeArea()
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(red: Double((hex >> 16) & 0xFF) / 255,
                  green: Double((hex >> 8) & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255)
    }
}

/// The two-part arrow from the Playwake icon.
struct PlaywakeLogo: View {
    var size: CGFloat = 32

    var body: some View {
        Canvas { context, canvas in
            let scale = canvas.width / 100
            func shape(_ points: [CGPoint]) -> Path {
                var path = Path()
                path.addLines(points.map { CGPoint(x: $0.x * scale, y: $0.y * scale) })
                path.closeSubpath()
                return path
            }
            context.fill(shape([.init(x: 20, y: 16), .init(x: 33, y: 16), .init(x: 82, y: 47),
                                .init(x: 28, y: 59), .init(x: 37, y: 47), .init(x: 20, y: 31)]),
                         with: .color(Color(hex: 0xA8F0CC)))
            context.fill(shape([.init(x: 20, y: 68), .init(x: 27, y: 61), .init(x: 82, y: 50),
                                .init(x: 36, y: 84), .init(x: 20, y: 84)]),
                         with: .color(Color(hex: 0x22E3B4)))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

/// Console power as the Xbox API reports it.
enum ConsolePower {
    case on, standby, offline

    init(_ raw: String) {
        switch raw.lowercased() {
        case "on": self = .on
        case "standby", "connectedstandby": self = .standby
        default: self = .offline
        }
    }

    var label: String {
        switch self {
        case .on: "On"
        case .standby: "Standby"
        case .offline: "Offline"
        }
    }

    var color: Color {
        switch self {
        case .on: Theme.mint
        case .standby: Theme.amber
        case .offline: Color(hex: 0x6B6E6E)
        }
    }
}

struct StatusDot: View {
    let color: Color

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 8, height: 8)
            .shadow(color: color == Theme.mint ? Theme.mint.opacity(0.8) : .clear, radius: 5)
    }
}

/// Cloud games come without artwork: a tile tinted from the title's hash with
/// its initials, the same rule as the mockups.
struct GameTile: View {
    let name: String
    var height: CGFloat = 104

    private var hue: Double {
        var hash = 0
        for scalar in name.unicodeScalars { hash = (hash * 31 + Int(scalar.value)) % 360 }
        return Double(hash)
    }

    private var initials: String {
        let words = name.split(separator: " ").filter { $0.first?.isLetter == true }
        return words.prefix(2).compactMap { $0.first.map(String.init) }.joined().uppercased()
    }

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            LinearGradient(colors: [Color(hue: hue / 360, saturation: 0.38, brightness: 0.42),
                                    Color(hue: (hue + 40).truncatingRemainder(dividingBy: 360) / 360,
                                          saturation: 0.30, brightness: 0.21)],
                           startPoint: .topLeading, endPoint: .bottomTrailing)
            Text(initials)
                .font(.system(size: 30, weight: .heavy))
                .foregroundStyle(.white.opacity(0.22))
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
                .padding(10)
            Text(name)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.text)
                .multilineTextAlignment(.leading)
                .lineLimit(2)
                .padding(12)
        }
        .frame(height: height)
        .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
    }
}

struct PrimaryButtonStyle: ButtonStyle {
    var prominent = true

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 17, weight: .semibold))
            .frame(maxWidth: .infinity, minHeight: 52)
            .foregroundStyle(prominent ? Theme.onMint : Theme.text)
            .background(prominent ? Theme.mint : Color.white.opacity(0.08),
                        in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            .opacity(configuration.isPressed ? 0.8 : 1)
    }
}

extension ButtonStyle where Self == PrimaryButtonStyle {
    static var playwake: PrimaryButtonStyle { PrimaryButtonStyle() }
    static var playwakeSecondary: PrimaryButtonStyle { PrimaryButtonStyle(prominent: false) }
}
