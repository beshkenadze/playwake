import SwiftUI

/// The stream: video from the moment streaming starts (the renderer attaches
/// when the view is created), a connecting screen until the first frame, an
/// optional statistics HUD and the stream menu.
struct StreamView: View {
    @ObservedObject var controller: SessionController
    @ObservedObject private var keyboardMouse = KeyboardMouse.shared

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            LowLatencyVideoView(renderer: controller.renderer)
                .ignoresSafeArea()
            if padActive && keyboardMouse.mouseConnected && keyboardMouse.mode == .desktop {
                PointerArea().ignoresSafeArea()
            }
            if !controller.firstFrame {
                ConnectingView(title: controller.streamTitle, status: controller.status) { controller.stop() }
            } else if controller.showStats {
                StatsHUD(stats: controller.stats)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
                    .padding(20)
                    .allowsHitTesting(false)
            }
            VirtualPadButtons()
            // Shown while connecting too: View+Menu opens the route's menu and
            // pauses the pad, so the menu must be visible whenever it is open.
            if controller.menuOpen {
                LinearGradient(colors: [Color.black.opacity(0.75), Color.black.opacity(0.3), .clear],
                               startPoint: .leading, endPoint: .trailing)
                    .ignoresSafeArea()
                    .allowsHitTesting(false)
                StreamMenu(controller: controller)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                    .padding(.vertical, 24)
                    .padding(.leading, 24)
            }
        }
        .contentShape(Rectangle())
        .onTapGesture {
            // A mouse click belongs to the stream; Cmd+K opens the menu.
            guard controller.firstFrame, !keyboardMouse.mouseConnected else { return }
            controller.menuOpen ? controller.closeMenu() : controller.openMenu()
        }
        .background { shortcuts }
        .onAppear {
            VirtualPad.shared.setActive(padActive)
            keyboardMouse.setStreamActive(padActive)
        }
        .onChange(of: padActive) { _, active in
            VirtualPad.shared.setActive(active)
            keyboardMouse.setStreamActive(active)
        }
        .onDisappear {
            VirtualPad.shared.setActive(false)
            keyboardMouse.setStreamActive(false)
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
    }

    /// The on-screen pad would cover the menu, which takes taps instead.
    private var padActive: Bool { controller.firstFrame && !controller.menuOpen }

    /// Keyboard shortcuts (Mac, iPad with a keyboard).
    private var shortcuts: some View {
        Group {
            Button("Stream menu") { controller.menuOpen ? controller.closeMenu() : controller.openMenu() }
                .keyboardShortcut("k", modifiers: .command)
            Button("Resume") { controller.closeMenu() }
                .keyboardShortcut(.escape, modifiers: [])
                .disabled(!controller.menuOpen)
            // Not while connecting: a new request would race the pending one.
            Button("Xbox Guide") { controller.perform(.guide) }
                .keyboardShortcut("g", modifiers: .command)
                .disabled(!controller.firstFrame)
            Button("Reconnect") { controller.perform(.reconnect) }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(!controller.firstFrame)
            Button("End stream") { controller.perform(.end) }
                .keyboardShortcut(".", modifiers: .command)
        }
        .opacity(0)
        .accessibilityHidden(true)
    }
}

private struct ConnectingView: View {
    let title: String
    let status: String
    let cancel: () -> Void

    var body: some View {
        ZStack {
            Theme.glow
            VStack(spacing: 16) {
                ProgressView()
                    .controlSize(.large)
                    .tint(Theme.mint)
                Text(title.isEmpty ? "Connecting" : "Connecting to \(title)")
                    .font(.system(size: 22, weight: .semibold))
                    .padding(.top, 6)
                Text(status)
                    .font(.footnote)
                    .foregroundStyle(Theme.muted)
                Button("Cancel", action: cancel)
                    .buttonStyle(.playwakeSecondary)
                    .frame(width: 160)
                    .padding(.top, 6)
            }
            .padding(24)
        }
    }
}

/// Small, without blur: it sits over live video and redraws once a second.
private struct StatsHUD: View {
    let stats: LabStats

    var body: some View {
        HStack(spacing: 6) {
            pill {
                StatusDot(color: stats.decodeFps > 0 ? Theme.mint : Theme.amber)
                    .scaleEffect(0.75)
                Text("\(stats.height)p · \(Int(stats.decodeFps.rounded())) fps")
            }
            pill {
                Text(network.isEmpty
                     ? "RTT \(Int(stats.rttMs.rounded())) ms"
                     : "\(network) · RTT \(Int(stats.rttMs.rounded())) ms")
            }
            pill { Text("Client \(Int(stats.clientMs.rounded())) ms") }
        }
        .font(.system(size: 12, weight: .semibold).monospacedDigit())
        .foregroundStyle(Color(hex: 0xDFE2E1))
    }

    private var network: String {
        switch stats.network {
        case "wifi": "Wi-Fi"
        case "ethernet": "Ethernet"
        case "cellular": "Cellular"
        default: ""
        }
    }

    private func pill<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        HStack(spacing: 6, content: content)
            .padding(.horizontal, 11)
            .frame(height: 28)
            .background(Color.black.opacity(0.55), in: Capsule())
    }
}

private struct StreamMenu: View {
    @ObservedObject var controller: SessionController
    @ObservedObject private var keyboardMouse = KeyboardMouse.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(controller.streamTitle).font(.system(size: 15, weight: .semibold))
                Spacer()
                Text("Input paused").font(.footnote).foregroundStyle(Theme.muted)
            }
            .padding(.horizontal, 6)
            .padding(.bottom, 8)
            row(.resume, "Resume")
            row(.guide, "Xbox Guide")
            row(.statistics, "Statistics") {
                Image(systemName: controller.showStats ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(controller.showStats ? Theme.mint : Theme.muted)
            }
            row(.mouse, "Mouse") {
                Text(keyboardMouse.mode == .game ? "Game" : "Desktop").foregroundStyle(Theme.muted)
            }
            row(.reconnect, "Reconnect")
            Spacer(minLength: 12)
            row(.end, "End stream", danger: true)
        }
        .padding(16)
        .frame(width: 300)
        .frame(maxHeight: 420)
        .background(.ultraThinMaterial.opacity(0.9), in: RoundedRectangle(cornerRadius: 26, style: .continuous))
        .background(Color(hex: 0x1C1F1E).opacity(0.6), in: RoundedRectangle(cornerRadius: 26, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 26, style: .continuous).stroke(Color.white.opacity(0.1)))
    }

    private func row(_ item: SessionController.MenuItem, _ title: String, danger: Bool = false,
                     @ViewBuilder trailing: () -> some View = { EmptyView() }) -> some View {
        let selected = SessionController.MenuItem.allCases.firstIndex(of: item) == controller.menuSelection
        return Button { controller.perform(item) } label: {
            HStack {
                Text(title)
                Spacer()
                trailing()
            }
            .font(.system(size: 16, weight: .medium))
            .foregroundStyle(selected ? Theme.onMint : (danger ? Theme.danger : Theme.text))
            .padding(.horizontal, 14)
            .frame(height: 46)
            .background(selected ? Theme.mint : .clear, in: RoundedRectangle(cornerRadius: 13, style: .continuous))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
