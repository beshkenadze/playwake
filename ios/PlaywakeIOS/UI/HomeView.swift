import GameController
import SwiftUI

/// Consoles, Xbox Cloud and Settings: a tab bar on iPhone, a sidebar on iPad
/// and Mac.
struct HomeView: View {
    @ObservedObject var controller: SessionController
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        if sizeClass == .compact {
            TabView(selection: $controller.section) {
                NavigationStack { ConsolesView(controller: controller) }
                    .tabItem { Label("Consoles", systemImage: "gamecontroller.fill") }
                    .tag(SessionController.Section.consoles)
                NavigationStack { CloudView(controller: controller) }
                    .tabItem { Label("Cloud", systemImage: "cloud.fill") }
                    .tag(SessionController.Section.cloud)
                NavigationStack { SettingsView(controller: controller) }
                    .tabItem { Label("Settings", systemImage: "gearshape.fill") }
                    .tag(SessionController.Section.settings)
            }
        } else {
            NavigationSplitView {
                List(selection: Binding<SessionController.Section?>(
                    get: { controller.section },
                    set: { if let section = $0 { controller.section = section } })) {
                    Label("Consoles", systemImage: "gamecontroller.fill")
                        .tag(SessionController.Section.consoles)
                    Label("Xbox Cloud", systemImage: "cloud.fill")
                        .tag(SessionController.Section.cloud)
                    Label("Settings", systemImage: "gearshape.fill")
                        .tag(SessionController.Section.settings)
                }
                .navigationTitle("Playwake")
                .navigationSplitViewColumnWidth(min: 220, ideal: 260)
            } detail: {
                NavigationStack {
                    switch controller.section {
                    case .consoles: ConsolesView(controller: controller)
                    case .cloud: CloudView(controller: controller)
                    case .settings: SettingsView(controller: controller)
                    }
                }
            }
        }
    }
}

// MARK: - Consoles

struct ConsolesView: View {
    @ObservedObject var controller: SessionController
    @Environment(\.horizontalSizeClass) private var sizeClass

    /// One column on iPhone; side by side on iPad and Mac, as many as consoles (up to two).
    private var columns: [GridItem] {
        let count = sizeClass == .compact ? 1 : max(1, min(controller.consoles.count, 2))
        return Array(repeating: GridItem(.flexible(), spacing: 14), count: count)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                if !controller.status.isEmpty {
                    Text(controller.status)
                        .font(.footnote)
                        .foregroundStyle(Theme.muted)
                }
                if controller.consoles.isEmpty {
                    VStack(alignment: .leading, spacing: 12) {
                        Text("No consoles on this account yet.")
                            .font(.headline)
                        Text("Turn on remote features on the Xbox: Settings › Devices & connections › Remote features.")
                            .font(.subheadline)
                            .foregroundStyle(Theme.muted)
                    }
                    .padding(22)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Theme.panel, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
                }
                LazyVGrid(columns: columns, spacing: 14) {
                    ForEach(controller.consoles) { console in
                        ConsoleCard(console: console) { controller.startHome(console) }
                    }
                }
                streamSummary.padding(.top, 12)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 8)
        }
        .background(Theme.canvas)
        .navigationTitle("Consoles")
        .toolbar {
            Button { controller.refreshConsoles() } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
        }
    }

    private var streamSummary: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Stream").foregroundStyle(Theme.muted)
                Spacer()
                Button("Edit") { controller.section = .settings }
            }
            .font(.system(size: 15))
            HStack(spacing: 8) {
                Chip(text: "\(controller.height)p")
                Chip(text: controller.playoutMs == 0 ? "Smoothing off" : "Smoothing \(controller.playoutMs) ms")
                Chip(text: controller.showStats ? "Stats on" : "Stats off")
            }
        }
    }
}

private struct Chip: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 14))
            .foregroundStyle(Color(hex: 0xD6D8D8))
            .padding(.horizontal, 14)
            .frame(height: 34)
            .background(Theme.card, in: Capsule())
    }
}

private struct ConsoleCard: View {
    let console: XboxConsole
    let play: () -> Void

    private var power: ConsolePower { ConsolePower(console.power) }
    private var hero: Bool { power == .on }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                StatusDot(color: power.color)
                Text("\(power.label) · \(console.model)")
            }
            .font(.system(size: 15))
            .foregroundStyle(Theme.muted)
            Text(console.name.isEmpty ? console.model : console.name)
                .font(.system(size: hero ? 28 : 22, weight: .bold))
                .padding(.top, 8)
            Spacer(minLength: 22)
            switch power {
            case .on:
                Button(action: play) { Label("Play", systemImage: "play.fill") }
                    .buttonStyle(.playwake)
            case .standby:
                Button("Wake and play", action: play)
                    .buttonStyle(PrimaryButtonStyle(prominent: false))
                    .foregroundStyle(Theme.mint)
            case .offline:
                Text("Turn the console on to play")
                    .font(.subheadline)
                    .foregroundStyle(Theme.muted)
                    .frame(minHeight: 52)
            }
        }
        .padding(22)
        .frame(maxWidth: .infinity, minHeight: 190, alignment: .leading)
        .background {
            RoundedRectangle(cornerRadius: 22, style: .continuous)
                .fill(hero
                      ? AnyShapeStyle(LinearGradient(colors: [Color(hex: 0x16302A), Color(hex: 0x13201D), Color(hex: 0x181B1B)],
                                                     startPoint: .topLeading, endPoint: .bottomTrailing))
                      : AnyShapeStyle(Theme.panel))
                .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous)
                    .stroke(hero ? Theme.mint.opacity(0.22) : Color.white.opacity(0.06)))
        }
    }
}

// MARK: - Cloud

struct CloudView: View {
    @ObservedObject var controller: SessionController
    @State private var query = ""

    private var games: [CloudGame] {
        let all = controller.cloud
        return query.isEmpty ? all : PlaywakeCore.shared.search(all.map(\.name), query: query).map { all[$0] }
    }

    var body: some View {
        let games = games
        Group {
            if controller.cloud.isEmpty {
                VStack(spacing: 14) {
                    ProgressView().tint(Theme.mint)
                    Text("Loading the cloud catalog…").foregroundStyle(Theme.muted)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                LibraryGrid(games: games, query: query) { controller.startCloud($0) }
            }
        }
        .background(Theme.canvas)
        .navigationTitle("Xbox Cloud")
        .searchable(text: $query, prompt: "Search \(controller.cloud.count) games")
    }
}

// MARK: - Settings

struct SettingsView: View {
    @ObservedObject var controller: SessionController
    @State private var controllerName = GCController.current?.vendorName
    @State private var confirmSignOut = false

    private var version: String {
        let app = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "–"
        return "\(app) · core \(PlaywakeCore.version)"
    }

    var body: some View {
        Form {
            Section {
                LabeledContent("Resolution") {
                    Picker("Resolution", selection: $controller.height) {
                        Text("720p").tag(UInt32(720))
                        Text("1080p").tag(UInt32(1080))
                        Text("1440p").tag(UInt32(1440))
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                }
                Picker("Smoothing", selection: Binding(get: { controller.playoutMs },
                                                       set: { controller.setPlayoutMs($0) })) {
                    Text("Off").tag(0)
                    ForEach([5, 10, 20, 50, 100, 150], id: \.self) { Text("\($0) ms").tag($0) }
                }
            } header: {
                Text("Stream")
            } footer: {
                Text("Smoothing absorbs Wi-Fi jitter. 10 ms costs almost nothing; higher values add delay. A new value applies the next time Playwake starts.")
            }
            #if targetEnvironment(macCatalyst)
            Section("Display") {
                Toggle("Sync to display refresh (V-Sync)", isOn: Binding(get: { controller.vsyncOn },
                                                                       set: { controller.setVsync($0) }))
            }
            #endif
            Section("Controls") {
                LabeledContent("Controller") {
                    HStack(spacing: 6) {
                        StatusDot(color: controllerName == nil ? Color(hex: 0x6B6E6E) : Theme.mint)
                        Text(controllerName ?? "Not connected")
                    }
                }
                #if targetEnvironment(macCatalyst)
                LabeledContent("Stream menu", value: "View + Menu or ⌘K")
                #else
                LabeledContent("Stream menu", value: "View + Menu or tap")
                #endif
            }
            Section("Diagnostics") {
                Toggle("Statistics overlay", isOn: $controller.showStats)
            }
            Section("Microsoft account") {
                LabeledContent("Signed in", value: controller.consoles.count == 1
                               ? "1 console" : "\(controller.consoles.count) consoles")
                Button("Sign out", role: .destructive) { confirmSignOut = true }
            }
            Section {
                LabeledContent("Version", value: version)
            }
        }
        .scrollContentBackground(.hidden)
        .background(Theme.canvas)
        .navigationTitle("Settings")
        .confirmationDialog("Sign out of Microsoft?", isPresented: $confirmSignOut, titleVisibility: .visible) {
            Button("Sign out", role: .destructive) { controller.signOut() }
        }
        .onReceive(NotificationCenter.default.publisher(for: .GCControllerDidConnect)) { _ in
            controllerName = GCController.current?.vendorName
        }
        .onReceive(NotificationCenter.default.publisher(for: .GCControllerDidDisconnect)) { _ in
            controllerName = GCController.current?.vendorName
        }
    }
}
