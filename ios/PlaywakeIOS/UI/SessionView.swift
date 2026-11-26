import SwiftUI
import UIKit

struct SessionView: View {
    @ObservedObject var controller: SessionController
    @Environment(\.openURL) private var openURL
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            if controller.streaming {
                stream
            } else {
                menu
            }
        }
        .onAppear { controller.restore() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background && controller.streaming {
                controller.stop()
            }
        }
    }

    // MARK: - Stream

    private var stream: some View {
        ZStack(alignment: .topLeading) {
            LowLatencyVideoView(renderer: controller.renderer)
                .ignoresSafeArea()
                .background(Color.black)

            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Circle()
                        .fill(controller.stats.decodeFps > 0 ? Color.green : Color.orange)
                        .frame(width: 8, height: 8)
                    Text("PLAYWAKE · XBOX")
                        .font(.system(size: 11, weight: .bold, design: .monospaced))
                }
                Text(controller.stats.summary)
                    .font(.system(size: 12, weight: .medium, design: .monospaced))
                    .fixedSize(horizontal: false, vertical: true)
                Text(controller.status)
                    .font(.system(size: 10, weight: .regular, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            .foregroundStyle(.white)
            .padding(10)
            .background(Color.black.opacity(0.55))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .padding(12)

            VStack {
                Spacer()
                HStack {
                    Spacer()
                    Button(action: { controller.stop() }) {
                        Label("Stop", systemImage: "stop.fill")
                            .font(.system(size: 15, weight: .semibold, design: .monospaced))
                            .padding(.horizontal, 18)
                            .padding(.vertical, 12)
                            .background(Color.white.opacity(0.16))
                            .clipShape(Capsule())
                    }
                    .foregroundStyle(.white)
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 18)
            }
        }
    }

    // MARK: - Menu

    private var menu: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                HStack(alignment: .firstTextBaseline) {
                    Text("PLAYWAKE")
                        .font(.system(size: 26, weight: .black, design: .monospaced))
                    Text("XBOX STREAMING")
                        .font(.system(size: 11, weight: .semibold, design: .monospaced))
                        .foregroundStyle(.secondary)
                    Spacer()
                    Text("core \(PlaywakeCore.version)")
                        .font(.system(size: 10, design: .monospaced))
                        .foregroundStyle(.tertiary)
                }

                switch controller.phase {
                case .restoring:
                    HStack(spacing: 10) {
                        ProgressView()
                        Text("Restoring your Xbox session…")
                            .font(.system(size: 13, design: .monospaced))
                    }
                case .signedOut, .failed:
                    signInPanel
                case .signingIn:
                    signingInPanel
                case .ready:
                    catalogs
                }

                if !controller.status.isEmpty {
                    Text(controller.status)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
            }
            .foregroundStyle(.white)
            .padding(24)
            .frame(maxWidth: 720, alignment: .leading)
        }
    }

    private var signInPanel: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Sign in with your Microsoft account to see your Xbox consoles and the cloud catalogue.")
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
            Button(action: { controller.signIn() }) {
                Label("Sign in with Xbox", systemImage: "person.crop.circle.badge.checkmark")
                    .font(.system(size: 15, weight: .semibold, design: .monospaced))
                    .padding(.horizontal, 18)
                    .padding(.vertical, 12)
                    .background(Color.white)
                    .foregroundStyle(.black)
                    .clipShape(Capsule())
            }
        }
    }

    private var signingInPanel: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let device = controller.device {
                Text("Scan the QR code or enter this code at Microsoft")
                    .font(.system(size: 13, weight: .semibold))
                HStack(alignment: .center, spacing: 20) {
                    if let url = device.url {
                        QRCodeView(text: url.absoluteString)
                    }
                    VStack(alignment: .leading, spacing: 10) {
                        Text(device.code)
                            .font(.system(size: 34, weight: .black, design: .monospaced))
                            .padding(.horizontal, 18)
                            .padding(.vertical, 10)
                            .background(Color.white.opacity(0.12))
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                            .textSelection(.enabled)
                        if device.prefilled {
                            Text("The link already includes the code")
                                .font(.system(size: 11, design: .monospaced))
                                .foregroundStyle(.secondary)
                        } else if let url = device.url {
                            Text(url.host ?? "microsoft.com/link")
                                .font(.system(size: 11, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                        HStack(spacing: 12) {
                            if let url = device.url {
                                Button(action: { openURL(url) }) {
                                    Label("Open Microsoft", systemImage: "safari")
                                }
                                .buttonStyle(.borderedProminent)
                            }
                            Button(action: {
                                UIPasteboard.general.string = device.code
                            }) {
                                Label("Copy code", systemImage: "doc.on.doc")
                            }
                            .buttonStyle(.bordered)
                        }
                        .font(.system(size: 13, weight: .medium, design: .monospaced))
                    }
                    Spacer(minLength: 0)
                }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { controller.cancelSignIn() }
                        .buttonStyle(.bordered)
                }
                .font(.system(size: 13, weight: .medium, design: .monospaced))
            } else {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Waiting for Microsoft…")
                        .font(.system(size: 13, design: .monospaced))
                }
                Button("Cancel", role: .cancel) { controller.cancelSignIn() }
                    .buttonStyle(.bordered)
            }
        }
    }

    @ViewBuilder
    private var catalogs: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Picker("Source", selection: $controller.target) {
                    Text("Consoles").tag(SessionController.Target.home)
                    Text("Cloud").tag(SessionController.Target.cloud)
                }
                .pickerStyle(.segmented)
                .frame(maxWidth: 280)

                Spacer()

                Picker("Resolution", selection: $controller.height) {
                    Text("720p").tag(UInt32(720))
                    Text("1080p").tag(UInt32(1080))
                    Text("1440p").tag(UInt32(1440))
                }
                .pickerStyle(.segmented)
                .frame(width: 220)

                Picker("Smoothing", selection: Binding(
                    get: { controller.playoutMs },
                    set: { controller.setPlayoutMs($0) })) {
                    Text("Off").tag(0)
                    Text("5 ms").tag(5)
                    Text("10 ms").tag(10)
                    Text("20 ms").tag(20)
                    Text("50 ms").tag(50)
                    Text("100 ms").tag(100)
                    Text("150 ms").tag(150)
                }
                .pickerStyle(.menu)
                .frame(width: 130)

                Toggle("V-Sync", isOn: Binding(
                    get: { controller.vsyncOn },
                    set: { controller.setVsync($0) }))
                    .toggleStyle(.switch)
                    .font(.system(size: 12, design: .monospaced))

                Button("Sign out") { controller.signOut() }
                    .font(.system(size: 12, design: .monospaced))
                    .buttonStyle(.bordered)
            }

            if controller.target == .home {
                rows(controller.consoles.map { console in
                    Row(id: console.id, title: console.name.isEmpty ? console.model : console.name,
                        subtitle: "\(console.model) · \(console.power)") {
                        controller.startHome(console)
                    }
                }, empty: "No consoles are linked to this account.")
            } else {
                rows(controller.cloud.map { game in
                    Row(id: game.id, title: game.name, subtitle: "Xbox Cloud Gaming") {
                        controller.startCloud(game)
                    }
                }, empty: "The cloud catalogue is empty.")
            }
        }
    }

    private struct Row {
        let id: String
        let title: String
        let subtitle: String
        let action: () -> Void
    }

    @ViewBuilder
    private func rows(_ items: [Row], empty: String) -> some View {
        if items.isEmpty {
            Text(empty)
                .font(.system(size: 12, design: .monospaced))
                .foregroundStyle(.secondary)
                .padding(.vertical, 10)
        } else {
            LazyVStack(spacing: 8) {
                ForEach(items, id: \.id) { row in
                    Button(action: row.action) {
                        HStack(spacing: 12) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(row.title)
                                    .font(.system(size: 14, weight: .semibold, design: .monospaced))
                                Text(row.subtitle)
                                    .font(.system(size: 10, design: .monospaced))
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            Image(systemName: "play.circle.fill")
                                .font(.system(size: 22))
                        }
                        .padding(.horizontal, 14)
                        .padding(.vertical, 12)
                        .background(Color.white.opacity(0.08))
                        .clipShape(RoundedRectangle(cornerRadius: 10))
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }
}
